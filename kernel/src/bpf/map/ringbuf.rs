//! Linux BPF ring records. Userspace sees two aliases of the same data pages;
//! kernel direct-map producers insert completed DISCARD padding before wrapping.
use super::util::BpfMapMeta;
use super::BpfMapCommonOps;
use crate::arch::MMArch;
use crate::bpf::map::util::round_up;
use crate::filesystem::epoll::{
    event_poll::{EPollItemList, EventPoll},
    EPollEventType, EPollItem,
};
use crate::filesystem::page_cache::PageCache;
use crate::libs::spinlock::SpinLock;
use crate::mm::allocator::page_frame::PageFrameCount;
use crate::mm::page::allocate_registered_intrinsic_unevictable_pages_exact;
use crate::mm::{MemoryManagementArch, VirtAddr};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use system_error::SystemError;

const PAGE_SIZE: usize = MMArch::PAGE_SIZE;
const HDR_BUSY_BIT: u32 = 1 << 31;
const HDR_DISCARD_BIT: u32 = 1 << 30;
const HDR_LEN_MASK: u32 = HDR_DISCARD_BIT - 1;
const REC_HDR_SIZE: usize = 8;
const WORD_BITS: usize = usize::BITS as usize;

struct RingRegistration {
    base: usize,
    size: usize,
    core: Weak<RingBufCore>,
}
fn ring_registry() -> &'static SpinLock<Vec<RingRegistration>> {
    static REGISTRY: SpinLock<Vec<RingRegistration>> = SpinLock::new(Vec::new());
    &REGISTRY
}
pub struct RingBufMap {
    core: Arc<RingBufCore>,
}
impl fmt::Debug for RingBufMap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RingBufMap")
            .field("data_sz", &self.core.memory.data_sz)
            .finish()
    }
}

/// Pointers are pinned by the core's cache. Only consumer_page is user-writable.
struct RingMemory {
    consumer_page: VirtAddr,
    pos_page: VirtAddr,
    data_base: VirtAddr,
    data_sz: usize,
}
struct ProducerState {
    producer: u64,
    pending: u64,
    /// One bit per aligned header offset, preallocated at map creation.
    active: Vec<usize>,
}
impl ProducerState {
    fn new(data_sz: usize) -> Result<Self, SystemError> {
        let words = (data_sz / REC_HDR_SIZE).div_ceil(WORD_BITS);
        let mut active = Vec::new();
        active
            .try_reserve_exact(words)
            .map_err(|_| SystemError::ENOMEM)?;
        active.resize(words, 0);
        Ok(Self {
            producer: 0,
            pending: 0,
            active,
        })
    }
    fn is_active(&self, off: usize) -> bool {
        let bit = off / REC_HDR_SIZE;
        self.active[bit / WORD_BITS] & (1usize << (bit % WORD_BITS)) != 0
    }
    fn set_active(&mut self, off: usize, active: bool) {
        let bit = off / REC_HDR_SIZE;
        let word = &mut self.active[bit / WORD_BITS];
        let mask = 1usize << (bit % WORD_BITS);
        if active {
            *word |= mask;
        } else {
            *word &= !mask;
        }
    }
}
pub struct RingBufCore {
    memory: RingMemory,
    state: SpinLock<ProducerState>,
    // A helper's temporary Arc pins the backing even during concurrent close.
    page_cache: Arc<PageCache>,
    epitems: EPollItemList,
}
impl RingMemory {
    fn consumer(&self) -> &AtomicU64 {
        unsafe { &*self.consumer_page.as_ptr::<AtomicU64>() }
    }
    fn producer(&self) -> &AtomicU64 {
        unsafe { &*self.pos_page.as_ptr::<AtomicU64>() }
    }
    fn header(&self, off: usize) -> &AtomicU32 {
        debug_assert!(off.is_multiple_of(REC_HDR_SIZE) && off <= self.data_sz - REC_HDR_SIZE);
        unsafe { &*self.data_base.as_ptr::<u8>().add(off).cast::<AtomicU32>() }
    }
    fn write_header(&self, off: usize, flags: u32) {
        // The second word is private metadata, unused by the consumer ABI.
        unsafe {
            core::ptr::write(self.data_base.as_ptr::<u8>().add(off + 4).cast::<u32>(), 0);
        }
        self.header(off).store(flags, Ordering::Relaxed);
    }
    fn record_offset(&self, ptr: *mut u8) -> Option<usize> {
        let off = (ptr as usize)
            .checked_sub(self.data_base.data())?
            .checked_sub(REC_HDR_SIZE)?;
        (off.is_multiple_of(REC_HDR_SIZE) && off <= self.data_sz - REC_HDR_SIZE).then_some(off)
    }
    fn advance_pending(&self, state: &mut ProducerState) {
        while state.pending != state.producer {
            let off = state.pending as usize & (self.data_sz - 1);
            let hdr = self.header(off).load(Ordering::Acquire);
            if hdr & HDR_BUSY_BIT != 0 {
                break;
            }
            let need = round_up((hdr & HDR_LEN_MASK) as usize + REC_HDR_SIZE, 8);
            state.pending = state.pending.wrapping_add(need as u64);
        }
    }
    fn has_space(&self, state: &ProducerState, cons: u64, need: usize) -> bool {
        let mask = (self.data_sz - 1) as u64;
        let next = state.producer.wrapping_add(need as u64);
        // A malicious consumer must not authorize overwriting a BUSY payload.
        next.wrapping_sub(cons) <= mask && next.wrapping_sub(state.pending) <= mask
    }
    fn reserve(&self, state: &mut ProducerState, size: usize) -> *mut u8 {
        if size > HDR_LEN_MASK as usize {
            return core::ptr::null_mut();
        }
        let need = round_up(REC_HDR_SIZE + size, 8);
        if need >= self.data_sz {
            return core::ptr::null_mut();
        }
        let cons = self.consumer().load(Ordering::Acquire);
        if !cons.is_multiple_of(REC_HDR_SIZE as u64)
            || state.producer.wrapping_sub(cons) > (self.data_sz - 1) as u64
        {
            return core::ptr::null_mut();
        }
        self.advance_pending(state);
        let mut off = state.producer as usize & (self.data_sz - 1);
        let tail = self.data_sz - off;
        if tail < need {
            if !self.has_space(state, cons, tail) {
                return core::ptr::null_mut();
            }
            // tail==8 is a valid, zero-payload completed DISCARD record.
            self.write_header(off, (tail - REC_HDR_SIZE) as u32 | HDR_DISCARD_BIT);
            state.producer = state.producer.wrapping_add(tail as u64);
            self.producer().store(state.producer, Ordering::Release);
            off = 0;
            // Publish padding alone if needed, so the consumer can free it
            // and a subsequent large reservation can make progress.
            self.advance_pending(state);
        }
        if !self.has_space(state, cons, need) {
            return core::ptr::null_mut();
        }
        self.write_header(off, size as u32 | HDR_BUSY_BIT);
        state.set_active(off, true);
        state.producer = state.producer.wrapping_add(need as u64);
        self.producer().store(state.producer, Ordering::Release);
        unsafe { self.data_base.as_ptr::<u8>().add(off + REC_HDR_SIZE) }
    }
    fn should_wake(&self, off: usize, flags: u64) -> bool {
        if flags & 2 != 0 {
            return true;
        }
        if flags & 1 != 0 {
            return false;
        }
        let cons = self.consumer().load(Ordering::Acquire) as usize & (self.data_sz - 1);
        if cons == off {
            return true;
        }
        // A wrapping reservation has a completed padding record before it.
        // The waiting reader may still be at that padding when submit runs.
        // Treat it as our record's start, while honoring NO_WAKEUP above.
        if off == 0 && cons != 0 && cons.is_multiple_of(REC_HDR_SIZE) {
            let header = self.header(cons).load(Ordering::Acquire);
            return header & (HDR_BUSY_BIT | HDR_DISCARD_BIT) == HDR_DISCARD_BIT
                && round_up((header & HDR_LEN_MASK) as usize + REC_HDR_SIZE, 8)
                    == self.data_sz - cons;
        }
        false
    }
    fn finish(&self, state: &mut ProducerState, ptr: *mut u8, discard: bool) -> Option<usize> {
        let off = self.record_offset(ptr)?;
        // Authenticate the exact live start before reading a pointer-derived
        // header. Interior, arbitrary and repeated pointers are ignored.
        if !state.is_active(off) {
            return None;
        }
        let hdr = self.header(off).load(Ordering::Relaxed);
        let flags = (hdr & HDR_LEN_MASK) | if discard { HDR_DISCARD_BIT } else { 0 };
        self.header(off).store(flags, Ordering::Release);
        state.set_active(off, false);
        Some(off)
    }
}
impl RingBufCore {
    pub fn data_size(&self) -> usize {
        self.memory.data_sz
    }
    pub fn reserve(&self, size: usize) -> *mut u8 {
        let (ptr, padding_only) = {
            let mut state = self.state.lock_irqsave();
            let before = state.producer;
            let ptr = self.memory.reserve(&mut state, size);
            (ptr, ptr.is_null() && before != state.producer)
        };
        // A completed padding-only publication must be drainable by an epoll
        // consumer even though no subsequent helper submit will occur.
        if padding_only {
            self.wake();
        }
        ptr
    }
    fn finish(&self, ptr: *mut u8, discard: bool, flags: u64) {
        let wake = {
            let mut state = self.state.lock_irqsave();
            match self.memory.finish(&mut state, ptr, discard) {
                Some(off) => self.memory.should_wake(off, flags),
                None => false,
            }
        };
        // EventPoll can call back into poll(): never hold the producer lock.
        if wake {
            self.wake();
        }
    }
    fn wake(&self) {
        let _ = EventPoll::wakeup_epoll(
            &self.epitems,
            EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM,
        );
    }
    pub fn poll_events(&self) -> usize {
        if self.memory.consumer().load(Ordering::Acquire)
            != self.memory.producer().load(Ordering::Acquire)
        {
            (EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize
        } else {
            0
        }
    }
    pub fn add_epitem(&self, epitem: Arc<EPollItem>) {
        self.epitems.add(epitem);
    }
    pub fn remove_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        self.epitems.remove(epitem)
    }
}
impl Drop for RingBufMap {
    fn drop(&mut self) {
        let ptr = Arc::as_ptr(&self.core);
        ring_registry()
            .lock_irqsave()
            .retain(|entry| entry.core.as_ptr() != ptr);
    }
}
/// Use only trusted registry range metadata before upgrading a single core.
/// In particular, never dereference a caller-provided sample-8 to find a ring.
fn ring_of(ptr: *mut u8) -> Option<Arc<RingBufCore>> {
    let registry = ring_registry().lock_irqsave();
    let entry = registry.iter().find(|entry| {
        (ptr as usize)
            .checked_sub(entry.base)
            .is_some_and(|off| off >= REC_HDR_SIZE && off <= entry.size)
    })?;
    entry.core.upgrade()
}
impl RingBufMap {
    pub fn new(meta: &BpfMapMeta) -> Result<Self, SystemError> {
        if meta.key_size != 0 || meta.value_size != 0 || meta._map_flags != 0 {
            return Err(SystemError::EINVAL);
        }
        let data_sz = meta.max_entries as usize;
        if data_sz < PAGE_SIZE || !data_sz.is_multiple_of(PAGE_SIZE) || !data_sz.is_power_of_two() {
            return Err(SystemError::EINVAL);
        }
        let state = ProducerState::new(data_sz)?;
        let data_pages = data_sz / PAGE_SIZE;
        let batch = allocate_registered_intrinsic_unevictable_pages_exact(PageFrameCount::new(
            2 + data_pages,
        ))?;
        let phy_addr = batch.start_paddr();
        for page in batch.pages() {
            page.write()
                .add_flags(crate::mm::page::PageFlags::PG_UPTODATE);
        }
        let virt = unsafe { MMArch::phys_2_virt(phy_addr) }.ok_or(SystemError::ENOMEM)?;
        let page_cache = PageCache::new_unowned(None, None);
        page_cache.adopt_preallocated_unevictable_batch(0, batch)?;
        for index in 0..data_pages {
            page_cache.alias_preallocated_unevictable_page(2 + index, 2 + data_pages + index)?;
        }
        let core = Arc::try_new(RingBufCore {
            memory: RingMemory {
                consumer_page: virt,
                pos_page: VirtAddr::new(virt.data() + PAGE_SIZE),
                data_base: VirtAddr::new(virt.data() + 2 * PAGE_SIZE),
                data_sz,
            },
            state: SpinLock::new(state),
            page_cache,
            epitems: EPollItemList::default(),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        let mut registry = ring_registry().lock_irqsave();
        registry.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        registry.push(RingRegistration {
            base: core.memory.data_base.data(),
            size: data_sz,
            core: Arc::downgrade(&core),
        });
        drop(registry);
        Ok(Self { core })
    }
    pub fn core(&self) -> &Arc<RingBufCore> {
        &self.core
    }
    pub fn page_cache(&self) -> Arc<PageCache> {
        self.core.page_cache.clone()
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
pub fn ringbuf_reserve(map: &super::BpfMap, size: u64, flags: u64) -> *mut u8 {
    if flags != 0 {
        return core::ptr::null_mut();
    }
    let ring = match map.ringbuf_core() {
        Some(core) => core,
        None => return core::ptr::null_mut(),
    };
    ring.reserve(size as usize)
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_submit/
pub fn ringbuf_submit(data_ptr: *mut u8, flags: u64) {
    if let Some(core) = ring_of(data_ptr) {
        core.finish(data_ptr, false, flags);
    }
}

/// See https://docs.ebpf.io/linux/helper-function/bpf_ringbuf_discard/
pub fn ringbuf_discard(data_ptr: *mut u8, flags: u64) {
    if let Some(core) = ring_of(data_ptr) {
        core.finish(data_ptr, true, flags);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Exercise the actual producer algorithm using ordinary aligned storage;
    // no allocator, PageManager, current task or page tables are needed.
    fn fixture() -> (Vec<u64>, RingMemory, ProducerState) {
        let mut backing = alloc::vec![0u64; 10];
        let base = backing.as_mut_ptr() as usize;
        let memory = RingMemory {
            consumer_page: VirtAddr::new(base),
            pos_page: VirtAddr::new(base + 8),
            data_base: VirtAddr::new(base + 16),
            data_sz: 64,
        };
        (backing, memory, ProducerState::new(64).unwrap())
    }

    #[test]
    fn linux_header_and_authenticated_finish() {
        let (_backing, memory, mut state) = fixture();
        let ptr = memory.reserve(&mut state, 8);
        assert!(!ptr.is_null());
        assert_eq!(memory.header(0).load(Ordering::Acquire), HDR_BUSY_BIT | 8);
        assert_eq!(
            memory.finish(&mut state, core::ptr::null_mut(), false),
            None
        );
        assert_eq!(
            memory.finish(&mut state, usize::MAX as *mut u8, false),
            None
        );
        assert_eq!(
            memory.finish(&mut state, unsafe { ptr.add(8) }, false),
            None
        );
        assert_eq!(memory.header(0).load(Ordering::Acquire), HDR_BUSY_BIT | 8);
        assert_eq!(memory.finish(&mut state, ptr, false), Some(0));
        assert_eq!(memory.header(0).load(Ordering::Acquire), 8);
        assert_eq!(memory.finish(&mut state, ptr, true), None);
        assert_eq!(memory.header(0).load(Ordering::Acquire), 8);
    }

    #[test]
    fn eight_byte_tail_is_completed_padding() {
        let (_backing, memory, mut state) = fixture();
        let first = memory.reserve(&mut state, 48); // 56-byte record, 8-byte tail
        memory.finish(&mut state, first, false);
        memory.consumer().store(56, Ordering::Release);
        let second = memory.reserve(&mut state, 8);
        assert_eq!(second as usize, memory.data_base.data() + REC_HDR_SIZE);
        assert_eq!(memory.header(56).load(Ordering::Acquire), HDR_DISCARD_BIT);
        assert_eq!(state.producer, 80);
        assert!(memory.should_wake(0, 0));
        assert!(!memory.should_wake(0, 1));
        assert!(memory.should_wake(0, 2));
        assert_eq!(memory.finish(&mut state, second, true), Some(0));
        assert_eq!(
            memory.header(0).load(Ordering::Acquire),
            HDR_DISCARD_BIT | 8
        );
    }

    #[test]
    fn padding_only_allows_next_large_reservation() {
        let (_backing, memory, mut state) = fixture();
        let first = memory.reserve(&mut state, 40); // need 48
        memory.finish(&mut state, first, false);
        memory.consumer().store(48, Ordering::Release);
        assert!(memory.reserve(&mut state, 40).is_null());
        assert_eq!(state.producer, 64);
        assert_eq!(
            memory.header(48).load(Ordering::Acquire),
            HDR_DISCARD_BIT | 8
        );
        memory.consumer().store(64, Ordering::Release);
        assert!(!memory.reserve(&mut state, 40).is_null());
        assert_eq!(state.producer, 112);
    }

    #[test]
    fn consumer_cannot_overwrite_pending_payload() {
        let (_backing, memory, mut state) = fixture();
        let first = memory.reserve(&mut state, 24); // need 32
                                                    // Userspace lies that it consumed an uncommitted record.
        memory.consumer().store(32, Ordering::Release);
        assert!(memory.reserve(&mut state, 24).is_null());
        assert_eq!(state.producer, 32);
        assert_eq!(memory.header(0).load(Ordering::Acquire), HDR_BUSY_BIT | 24);
        memory.finish(&mut state, first, false);
        assert!(!memory.reserve(&mut state, 24).is_null());
    }

    #[test]
    fn invalid_consumer_and_counter_wrap() {
        let (_backing, memory, mut state) = fixture();
        for cons in [1, 8, u64::MAX - 63] {
            memory.consumer().store(cons, Ordering::Release);
            assert!(memory.reserve(&mut state, 8).is_null());
            assert_eq!(state.producer, 0);
        }
        state.producer = u64::MAX - 7;
        state.pending = state.producer;
        memory.consumer().store(state.producer, Ordering::Release);
        let ptr = memory.reserve(&mut state, 8);
        assert_eq!(ptr as usize, memory.data_base.data() + REC_HDR_SIZE);
        assert_eq!(state.producer, 16);
        assert_eq!(state.pending, 0);
        assert_eq!(memory.header(56).load(Ordering::Acquire), HDR_DISCARD_BIT);
    }

    #[test]
    fn zero_length_reservation_has_a_real_header() {
        let (_backing, memory, mut state) = fixture();
        let ptr = memory.reserve(&mut state, 0);
        assert!(!ptr.is_null());
        assert_eq!(state.producer, 8);
        assert_eq!(memory.header(0).load(Ordering::Acquire), HDR_BUSY_BIT);
        assert_eq!(memory.finish(&mut state, ptr, false), Some(0));
        assert_eq!(memory.header(0).load(Ordering::Acquire), 0);
    }
}
