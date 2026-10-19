//! BPF_MAP_TYPE_RINGBUF — the shared-ring event channel used by tracing
//! agents (agentrace's primary event transport).
//!
//! Memory layout (one `PageCache`, filled at map creation):
//!
//! ```text
//! page-cache index 0        : consumer page  — u64 consumer_pos (user rw, kernel ro)
//! page-cache index 1        : pos page       — u64 producer_pos (kernel rw, user ro)
//! page-cache index 2..2+N   : data ring      — N pages of records (kernel rw, user ro)
//! ```
//!
//! A record is `u32 len<<2 | BUSY | DISCARD`, a second `u32` owner tag
//! (the ring registration id used by submit/discard to find the ring back
//! from a raw data pointer), then `round_up(len, 8)` payload bytes.
//!
//! Linux double-maps the data area so a record can straddle the wrap point;
//! here a record that does not fit the ring tail is preceded by a DISCARD
//! padding record and written from offset 0 instead. Consumer-side walkers
//! (libbpf/aya) iterate headers, so padding records are skipped the same way
//! Linux's own padding records are, and the second half of the user mapping
//! is never touched.

use core::fmt;
use core::sync::atomic::{AtomicU32, Ordering};

use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::MMArch;
use crate::bpf::map::util::round_up;
use crate::filesystem::page_cache::PageCache;
use crate::libs::spinlock::SpinLock;
use crate::mm::allocator::page_frame::PageFrameCount;
use crate::mm::page::allocate_registered_intrinsic_unevictable_pages_exact;
use crate::mm::{MemoryManagementArch, VirtAddr};

use super::BpfMapCommonOps;
use super::util::BpfMapMeta;

const PAGE_SIZE: usize = MMArch::PAGE_SIZE;
/// 8-byte record header: u32 len|flags + u32 owner tag.
/// `len` low bits: record is being written (userspace must not consume it).
const HDR_BUSY_BIT: u32 = 1;
/// `len` low bits: record content is meaningless, skip it.
const HDR_DISCARD_BIT: u32 = 2;
/// 8-byte record header: u32 len|flags + u32 owner tag.
const REC_HDR_SIZE: usize = 8;

/// Every live ring registers itself here so `bpf_ringbuf_submit/discard`
/// can recover the owning ring from a raw payload pointer.
fn ring_registry() -> &'static SpinLock<Vec<(u32, Arc<RingBufCore>)>> {
    static REGISTRY: SpinLock<Vec<(u32, Arc<RingBufCore>)>> = SpinLock::new(Vec::new());
    &REGISTRY
}

static NEXT_RING_ID: AtomicU32 = AtomicU32::new(1);

pub struct RingBufMap {
    core: Arc<RingBufCore>,
    page_cache: Arc<PageCache>,
}

impl fmt::Debug for RingBufMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingBufMap")
            .field("ring_id", &self.core.id)
            .field("data_sz", &self.core.data_sz)
            .finish()
    }
}

/// The producer side of the ring. `producer_pos`/`consumer_pos` live inside
/// the mapped pages themselves (kernel accesses them through the direct
/// map) so the user-visible copies are always authoritative.
pub struct RingBufCore {
    /// Kernel virtual base of the consumer page (cache index 0).
    consumer_page: VirtAddr,
    /// Kernel virtual base of the pos page (cache index 1): producer_pos u64.
    pos_page: VirtAddr,
    /// Kernel virtual base of the data ring (cache index 2..).
    data_base: VirtAddr,
    /// data ring byte size, power of two.
    data_sz: usize,
    /// data_sz - 1
    mask: usize,
    /// Serializes producer-side reservation (multi-CPU safe; contention is
    /// irrelevant at trace-event rates).
    prod_lock: SpinLock<()>,
    /// Registration id echoed in every record header.
    id: u32,
}

impl RingBufCore {
    #[inline]
    fn consumer_pos(&self) -> u64 {
        unsafe { core::ptr::read_volatile(self.consumer_page.as_ptr::<u8>() as *const u64) }
    }

    #[inline]
    fn producer_pos(&self) -> u64 {
        unsafe { core::ptr::read_volatile(self.pos_page.as_ptr::<u8>() as *const u64) }
    }

    #[inline]
    fn set_producer_pos(&self, pos: u64) {
        unsafe {
            core::ptr::write_volatile(self.pos_page.as_ptr::<u8>() as *mut u64, pos);
        }
    }
    #[inline]
    fn write_hdr(&self, off: usize, len_flags: u32, owner: u32) {
        let hdr = unsafe {
            core::slice::from_raw_parts_mut(self.data_base.as_ptr::<u8>().add(off) as *mut u32, 2)
        };
        hdr[0] = len_flags;
        hdr[1] = owner;
    }

    /// Data-ring byte size (the map's max_entries).
    pub fn data_size(&self) -> usize {
        self.data_sz
    }

    #[inline]
    fn hdr_at(&self, off: usize) -> (u32, u32) {
        let hdr = unsafe {
            core::slice::from_raw_parts(self.data_base.as_ptr::<u8>().add(off) as *const u32, 2)
        };
        (hdr[0], hdr[1])
    }

    /// Reserve `size` payload bytes. Returns the payload address inside the
    /// ring, or 0 when the ring is full. The record is published as BUSY;
    /// `submit`/`discard` finish it.
    pub fn reserve(&self, size: usize) -> *mut u8 {
        if size == 0 || size > u32::MAX as usize / 4 {
            return core::ptr::null_mut();
        }
        let need = REC_HDR_SIZE + round_up(size, 8);
        if need > self.data_sz {
            return core::ptr::null_mut();
        }

        let _guard = self.prod_lock.lock();
        let cons = self.consumer_pos();
        let mut prod = self.producer_pos();
        let used = prod - cons;

        // A record must not straddle the wrap point: if the tail cannot hold
        // header+payload, first reserve a DISCARD pad for the whole tail.
        let mut off = (prod & self.mask as u64) as usize;
        let mut pad = 0usize;
        if off + need > self.data_sz {
            pad = self.data_sz - off - REC_HDR_SIZE;
        }
        let total_need = need + if pad > 0 { REC_HDR_SIZE + round_up(pad, 8) } else { 0 };
        if self.data_sz as u64 - used < total_need as u64 {
            return core::ptr::null_mut();
        }

        if pad > 0 {
            self.write_hdr(off, ((pad as u32) << 2) | HDR_BUSY_BIT | HDR_DISCARD_BIT, self.id);
            prod += (REC_HDR_SIZE + round_up(pad, 8)) as u64;
            off = 0;
        }

        self.write_hdr(off, ((size as u32) << 2) | HDR_BUSY_BIT, self.id);
        // Reserve the slot: userspace observes the record as BUSY until
        // submit/discard releases it.
        self.set_producer_pos(prod + need as u64);
        unsafe { self.data_base.as_ptr::<u8>().add(off + REC_HDR_SIZE) }
    }

    /// Look up the header that owns `data_ptr` and rewrite its flags.
    fn finish(&self, data_ptr: *mut u8, discard: bool) {
        let off = (data_ptr as usize - self.data_base.data()) - REC_HDR_SIZE;
        let (_, owner) = self.hdr_at(off);
        debug_assert_eq!(owner, self.id);
        let (hdr, _) = self.hdr_at(off);
        let len = hdr >> 2;
        let flags = if discard {
            (len << 2) | HDR_DISCARD_BIT
        } else {
            len << 2
        };
        let word = unsafe { self.data_base.as_ptr::<u8>().add(off) as *mut u32 };
        // Release: the payload is complete before the BUSY bit disappears.
        unsafe {
            core::ptr::write_volatile(word, flags);
        }
        core::sync::atomic::compiler_fence(Ordering::SeqCst);
    }
}

impl Drop for RingBufCore {
    fn drop(&mut self) {
        let mut reg = ring_registry().lock();
        if let Some(pos) = reg.iter().position(|(id, _)| *id == self.id) {
            reg.remove(pos);
        }
    }
}

/// Resolve a payload pointer written by `reserve` back to its ring.
fn ring_of(data_ptr: *mut u8) -> Option<Arc<RingBufCore>> {
    let hdr_ptr = (data_ptr as usize).checked_sub(REC_HDR_SIZE)?;
    let off = unsafe { core::ptr::read_volatile(hdr_ptr as *const u32) };
    let _len = off >> 2;
    let owner = unsafe { core::ptr::read_volatile((hdr_ptr + 4) as *const u32) };
    let reg = ring_registry().lock();
    reg.iter()
        .find(|(id, _)| *id == owner)
        .map(|(_, core)| core.clone())
}

impl RingBufMap {
    pub fn new(meta: &BpfMapMeta) -> Result<Self, SystemError> {
        // Linux ringbuf_map_alloc: key/value sizes are meaningless and the
        // ring size must be page-aligned and a power of two.
        if meta.key_size != 0 || meta.value_size != 0 {
            return Err(SystemError::EINVAL);
        }
        let data_sz = meta.max_entries as usize;
        if data_sz < PAGE_SIZE || data_sz % PAGE_SIZE != 0 || !data_sz.is_power_of_two() {
            return Err(SystemError::EINVAL);
        }

        let total_pages = 2 + data_sz / PAGE_SIZE;
        let batch = allocate_registered_intrinsic_unevictable_pages_exact(PageFrameCount::new(
            total_pages,
        ))?;
        let phy_addr = batch.start_paddr();
        for page in batch.pages() {
            page.write().add_flags(crate::mm::page::PageFlags::PG_UPTODATE);
        }
        let virt = unsafe { MMArch::phys_2_virt(phy_addr) }.ok_or(SystemError::ENOMEM)?;

        let page_cache = PageCache::new_unowned(None, None);
        page_cache.adopt_preallocated_unevictable_batch(0, batch)?;

        let id = NEXT_RING_ID.fetch_add(1, Ordering::Relaxed);
        let core = Arc::new(RingBufCore {
            consumer_page: virt,
            pos_page: VirtAddr::new(virt.data() + PAGE_SIZE),
            data_base: VirtAddr::new(virt.data() + 2 * PAGE_SIZE),
            data_sz,
            mask: data_sz - 1,
            prod_lock: SpinLock::new(()),
            id,
        });
        core.set_producer_pos(0);
        // Zero the consumer position once; the pages arrive zeroed.
        unsafe {
            core::ptr::write_volatile(core.consumer_page.as_ptr::<u8>() as *mut u64, 0);
        }
        ring_registry().lock().push((id, core.clone()));

        Ok(RingBufMap { core, page_cache })
    }

    pub fn core(&self) -> &Arc<RingBufCore> {
        &self.core
    }

    pub fn page_cache(&self) -> Arc<PageCache> {
        self.page_cache.clone()
    }
}

impl BpfMapCommonOps for RingBufMap {
    fn lookup_elem(&mut self, _key: &[u8]) -> super::Result<Option<&[u8]>> {
        Err(SystemError::ENOSYS)
    }
    fn get_next_key(&self, _key: Option<&[u8]>, _next_key: &mut [u8]) -> super::Result<()> {
        Err(SystemError::ENOSYS)
    }
}

// ---------------------------------------------------------------------------
// Helper entry points (bpf_ringbuf_reserve/submit/discard/output)
// ---------------------------------------------------------------------------

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_reserve/
pub fn ringbuf_reserve(map: &super::BpfMap, size: u64, _flags: u64) -> *mut u8 {
    let ring = match map.ringbuf_core() {
        Some(core) => core,
        None => return core::ptr::null_mut(),
    };
    ring.reserve(size as usize)
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_submit/
pub fn ringbuf_submit(data_ptr: *mut u8, _flags: u64) {
    if let Some(core) = ring_of(data_ptr) {
        core.finish(data_ptr, false);
    }
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_discard/
pub fn ringbuf_discard(data_ptr: *mut u8, _flags: u64) {
    if let Some(core) = ring_of(data_ptr) {
        core.finish(data_ptr, true);
    }
}
