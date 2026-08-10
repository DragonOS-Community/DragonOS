//! TPACKET V1/V2 mmap ring buffer for AF_PACKET sockets.
//!
//! The ring is a contiguous array of fixed-size frames carved out of a set of
//! physically-contiguous pages allocated at `setsockopt(PACKET_RX_RING)` time.
//! Those pages are inserted into a [`PageCache`] so that the generic mmap
//! page-fault path (`PageFaultHandler::filemap_map_pages`) maps them into
//! userspace on demand.  The kernel side writes frames through the linear
//! kernel virtual address returned by `phys_2_virt`, sharing the same physical
//! pages with userspace — zero-copy packet capture.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use crate::libs::mutex::Mutex;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::mm::LockedFrameAllocator;
use crate::arch::MMArch;
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::file::File;
use crate::filesystem::vfs::{FileSystem, FsInfo, IndexNode, SuperBlock, VmaOpenRollback};
use crate::mm::allocator::page_frame::PageFrameCount;
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::page::{page_manager_lock, PageFlags, PageType};
use crate::mm::MemoryManagementArch;
use crate::mm::VmFaultReason;
use crate::mm::{VirtRegion, VmFlags};

use super::uapi::{
    Tpacket2Hdr, TpacketHdr, TP_STATUS_KERNEL, TP_STATUS_LOSING, TP_STATUS_USER,
    TP_STATUS_VLAN_TPID_VALID, TP_STATUS_VLAN_VALID,
};
use super::{PacketMetadata, PacketSocketType};

const PAGE_SIZE: usize = MMArch::PAGE_SIZE;

// ---------------------------------------------------------------------------
// Fake filesystem — provides fault/map_pages that delegate to the generic
// filemap helpers, exactly like the perf subsystem's PerfFakeFs.
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct PacketFakeFs;

impl FileSystem for PacketFakeFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        panic!("PacketFakeFs has no root inode")
    }
    fn info(&self) -> FsInfo {
        panic!("PacketFakeFs has no fs info")
    }
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
    fn name(&self) -> &str {
        "packet"
    }
    fn super_block(&self) -> SuperBlock {
        panic!("PacketFakeFs has no super block")
    }
    unsafe fn fault(&self, pfm: &mut PageFaultMessage) -> VmFaultReason {
        PageFaultHandler::filemap_fault(pfm)
    }
    unsafe fn page_mkwrite(&self, _pfm: &mut PageFaultMessage) -> VmFaultReason {
        // Ring pages are pre-allocated PageType::Normal (not disk-backed), so
        // the generic filemap_page_mkwrite — which requires PageType::File and
        // prepares writeback — would wrongly SIGBUS. Match PerfFakeFs: succeed
        // and let do_wp_page upgrade the PTE in place.
        VmFaultReason::empty()
    }
    unsafe fn map_pages(
        &self,
        pfm: &mut PageFaultMessage,
        start_pgoff: usize,
        end_pgoff: usize,
    ) -> VmFaultReason {
        PageFaultHandler::filemap_map_pages(pfm, start_pgoff, end_pgoff)
    }
    fn vma_open(
        &self,
        file: &Arc<File>,
        _region: VirtRegion,
        _vm_flags: VmFlags,
    ) -> VmaOpenRollback {
        use super::PacketSocket;
        if let Some(socket) = file.inode().as_any_ref().downcast_ref::<PacketSocket>() {
            socket.ring_vma_opened();
            VmaOpenRollback::Close
        } else {
            VmaOpenRollback::NotRequired
        }
    }
    fn vma_close(&self, file: &Arc<File>, _region: VirtRegion, _vm_flags: VmFlags) {
        use super::PacketSocket;
        if let Some(socket) = file.inode().as_any_ref().downcast_ref::<PacketSocket>() {
            socket.ring_vma_closed();
        }
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

// TpacketVersion, RingConfig are re-exported from the standalone `tpacket`
// crate for host-testable ABI and validation logic.
pub use tpacket::{RingConfig, TpacketVersion};

/// Bounded raw writer for a frame concurrently shared with userspace.
///
/// Deliberately does not create `&mut [u8]`: the kernel cannot prove exclusive
/// access to an mmap'ed frame. Every operation checks its byte range before
/// performing a raw write.
struct FrameWriter {
    base: *mut u8,
    capacity: usize,
}

impl FrameWriter {
    /// # Safety
    ///
    /// `base..base + capacity` must remain a live, writable, contiguous kernel
    /// mapping for the lifetime of this writer. Concurrent userspace aliases
    /// are allowed; callers must obey the TPACKET ownership protocol.
    unsafe fn new(base: usize, capacity: usize) -> Self {
        Self {
            base: base as *mut u8,
            capacity,
        }
    }

    fn checked_ptr(&self, offset: usize, len: usize) -> Option<*mut u8> {
        let end = offset.checked_add(len)?;
        if end > self.capacity {
            return None;
        }
        Some(unsafe { self.base.add(offset) })
    }

    fn write<T: Copy>(&self, offset: usize, value: T) -> Option<()> {
        let dst = self.checked_ptr(offset, core::mem::size_of::<T>())?;
        unsafe { core::ptr::write_unaligned(dst.cast::<T>(), value) };
        Some(())
    }

    fn copy_slice(&self, offset: usize, source: &[u8]) -> Option<()> {
        let dst = self.checked_ptr(offset, source.len())?;
        unsafe { core::ptr::copy(source.as_ptr(), dst, source.len()) };
        Some(())
    }

    fn zero(&self, offset: usize, len: usize) -> Option<()> {
        let dst = self.checked_ptr(offset, len)?;
        unsafe { core::ptr::write_bytes(dst, 0, len) };
        Some(())
    }
}

/// Result of attempting to write a packet into the ring.
pub enum RingWriteResult {
    /// A frame was filled and published (status KERNEL→USER).
    Written,
    /// Every frame is still owned by userspace (TP_STATUS_USER); packet dropped.
    Dropped,
}

#[derive(Debug)]
pub struct RingState {
    pub version: TpacketVersion,
    pub reserve: u32,
    pub ring: Option<Arc<Mutex<PacketRing>>>,
    /// Number of active VMA mappings covering the ring. Teardown returns EBUSY
    /// while this is non-zero, matching Linux `mapped` accounting.
    pub mapped: usize,
}

impl RingState {
    pub fn new() -> Self {
        Self {
            version: TpacketVersion::V1,
            reserve: 0,
            ring: None,
            mapped: 0,
        }
    }
}
/// V1/V2 receive ring buffer.
#[derive(Debug)]
pub struct PacketRing {
    config: RingConfig,
    version: TpacketVersion,
    raw: bool,
    /// Per-block kernel virtual address. Each block is a physically-contiguous
    /// run of `block_size` bytes, but different blocks need not be contiguous
    /// with each other — this mirrors Linux `alloc_pg_vec` and avoids a single
    /// `block_nr * block_size` contiguous allocation that may ENOMEM under
    /// memory fragmentation (plan §5 Task 3, evaluation P2-4 fix).
    block_vaddrs: Vec<usize>,
    frames_per_block: usize,
    total_size: usize,
    page_cache: Arc<PageCache>,
    head: u32,
    reserve: usize,
}

impl PacketRing {
    /// Allocate the ring pages, zero them (so every frame starts as
    /// `TP_STATUS_KERNEL = 0`), insert them into a fresh page cache, and return
    /// the ready ring together with the page cache for mmap wiring.
    pub fn setup(
        config: RingConfig,
        version: TpacketVersion,
        sock_type: PacketSocketType,
        reserve: usize,
    ) -> Result<(Self, Arc<PageCache>), SystemError> {
        let total_size = config.block_nr * config.block_size;
        let pages_per_block = config.block_size / PAGE_SIZE;
        // PageCache::new already returns Arc<PageCache>.
        let page_cache: Arc<PageCache> = PageCache::new(None, None);
        let mut block_vaddrs = Vec::new();
        block_vaddrs
            .try_reserve_exact(config.block_nr)
            .map_err(|_| SystemError::ENOMEM)?;

        // Per-block allocation (matches Linux alloc_pg_vec): each block is an
        // independent `block_size` contiguous physical run. This avoids one
        // large `block_nr * block_size` allocation that fails under fragmented
        // memory.
        for block_idx in 0..config.block_nr {
            let (phy_addr, mut pages) = {
                let mut pm = page_manager_lock();
                pm.create_pages(
                    PageType::Normal,
                    PageFlags::PG_UNEVICTABLE,
                    &mut LockedFrameAllocator,
                    PageFrameCount::new(pages_per_block),
                )?
            };
            if pages.len() < pages_per_block {
                let mut pm = page_manager_lock();
                for page in &pages {
                    pm.remove_page(&page.phys_address());
                }
                drop(pm);
                return Err(SystemError::ENOMEM);
            }

            // Buddy may round the allocation up. Detach the unused tail from
            // PageManager under its lock, then drop the final references only
            // after releasing the lock.
            if pages.len() > pages_per_block {
                {
                    let mut pm = page_manager_lock();
                    for page in &pages[pages_per_block..] {
                        pm.remove_page(&page.phys_address());
                    }
                }
                pages.truncate(pages_per_block);
            }

            let vaddr = match unsafe { MMArch::phys_2_virt(phy_addr) } {
                Some(vaddr) => vaddr.data(),
                None => {
                    let mut pm = page_manager_lock();
                    for page in &pages {
                        pm.remove_page(&page.phys_address());
                    }
                    drop(pm);
                    return Err(SystemError::EFAULT);
                }
            };

            for j in 0..pages_per_block {
                let page = &pages[j];
                page.write().add_flags(PageFlags::PG_UPTODATE);
                if let Err(err) = page_cache.insert_preallocated_unevictable_page(
                    block_idx * pages_per_block + j,
                    page.clone(),
                ) {
                    let mut pm = page_manager_lock();
                    for page in &pages {
                        pm.remove_page(&page.phys_address());
                    }
                    drop(pm);
                    return Err(err);
                }
            }
            // create_pages() zeroed the whole actual extent before publishing
            // any Page object, so every frame already starts KERNEL-owned.
            block_vaddrs.push(vaddr);
        }

        let ring = Self {
            config,
            version,
            raw: sock_type == PacketSocketType::Raw,
            block_vaddrs,
            frames_per_block: config.block_size / config.frame_size,
            total_size,
            page_cache: page_cache.clone(),
            head: 0,
            reserve,
        };
        Ok((ring, page_cache))
    }

    pub fn total_size(&self) -> usize {
        self.total_size
    }

    pub fn page_cache(&self) -> &Arc<PageCache> {
        &self.page_cache
    }

    /// Match Linux packet_poll(): readiness is determined from the frame just
    /// before the producer head, not by scanning the entire ring.
    pub fn has_user_frames(&self) -> bool {
        let previous = if self.head == 0 {
            self.config.frame_nr - 1
        } else {
            self.head as usize - 1
        };
        self.read_tp_status(self.frame_base(previous)) != TP_STATUS_KERNEL
    }

    /// Write one packet into the ring. Caller must hold the ring lock.
    ///
    /// `filter_snaplen` is the cBPF-limited visible length (already clamped to
    /// `wire_len` by the caller).  `losing` requests TP_STATUS_LOSING on the
    /// published frame (set while `stats_drops > 0`).
    pub(super) fn write_frame(
        &mut self,
        input: &super::rx::PacketFilterInput,
        meta: &PacketMetadata,
        filter_snaplen: usize,
        losing: bool,
    ) -> RingWriteResult {
        let hdrlen = self.version.hdrlen();
        // Derive the visible MAC length from the normalized capture view's
        // network offset — not from VLAN metadata presence.  Inbound
        // normalized VLAN has a 14-byte visible MAC (tag stripped); outbound
        // inline VLAN has an 18-byte visible MAC (tag retained).
        let mac_len = meta.net_offset;
        let Some(offsets) =
            tpacket::calculate_frame_offsets(hdrlen, mac_len, self.reserve, self.raw)
        else {
            return RingWriteResult::Dropped;
        };
        let netoff = offsets.netoff as usize;
        let data_off = offsets.macoff as usize;
        let Some(data_cap) = self.config.frame_size.checked_sub(data_off) else {
            return RingWriteResult::Dropped;
        };
        if data_cap == 0 {
            return RingWriteResult::Dropped;
        }

        // O(1) single-head check: Linux never searches past a busy head slot.
        // If the current head is USER-owned, the packet is dropped.
        let base = self.frame_base(self.head as usize);
        if self.read_tp_status(base) != TP_STATUS_KERNEL {
            return RingWriteResult::Dropped;
        }

        let Some(status) = self.fill_frame(
            base,
            input,
            meta,
            netoff,
            data_off,
            data_cap,
            filter_snaplen,
        ) else {
            return RingWriteResult::Dropped;
        };

        // Publish: flip status KERNEL→USER *last*, with Release ordering so the
        // data writes above are visible before userspace observes USER.
        let mut final_status = status;
        if losing {
            final_status |= TP_STATUS_LOSING;
        }
        self.publish(base, final_status);

        self.head = ((self.head as usize + 1) % self.config.frame_nr) as u32;
        RingWriteResult::Written
    }

    // -- helpers ----------------------------------------------------------

    #[inline]
    fn frame_base(&self, index: usize) -> usize {
        // Frames are laid out flat within each block. Block b occupies
        // [block_vaddrs[b], block_vaddrs[b] + block_size), and frames inside
        // it are `frame_size` apart. Different blocks need not be physically
        // contiguous.
        let block_idx = index / self.frames_per_block;
        let block_offset = (index % self.frames_per_block) * self.config.frame_size;
        self.block_vaddrs[block_idx] + block_offset
    }

    /// Read `tp_status` (works for both V1 u64 and V2 u32 — low 32 bits carry
    /// the status flags that matter).
    fn read_tp_status(&self, frame_base: usize) -> u32 {
        match self.version {
            TpacketVersion::V1 => {
                let a = unsafe { &*(frame_base as *const AtomicU64) };
                a.load(Ordering::Acquire) as u32
            }
            TpacketVersion::V2 => {
                let a = unsafe { &*(frame_base as *const AtomicU32) };
                a.load(Ordering::Acquire)
            }
        }
    }

    fn publish(&self, frame_base: usize, status: u32) {
        match self.version {
            TpacketVersion::V1 => {
                let a = unsafe { &*(frame_base as *const AtomicU64) };
                a.store(status as u64, Ordering::Release);
            }
            TpacketVersion::V2 => {
                let a = unsafe { &*(frame_base as *const AtomicU32) };
                a.store(status, Ordering::Release);
            }
        }
    }

    /// Fill the header and copy packet data into the frame at `base`, using the
    /// normalized capture view so DGRAM/VLAN layout matches the queue path.
    #[allow(clippy::too_many_arguments)]
    fn fill_frame(
        &self,
        base: usize,
        input: &super::rx::PacketFilterInput,
        meta: &PacketMetadata,
        netoff: usize,
        data_off: usize,
        data_cap: usize,
        filter_snaplen: usize,
    ) -> Option<u32> {
        let is_vlan = meta.vlan_tpid != 0;

        // tp_len = original socket-visible length (not truncated by filter).
        // tp_snaplen = min(filter result, wire_len, frame capacity).
        let wire_len = meta.wire_len;
        let snaplen = filter_snaplen.min(wire_len).min(data_cap);

        let tp_mac = u16::try_from(data_off).ok()?;
        let tp_net = u16::try_from(netoff).ok()?;
        let tp_len = u32::try_from(wire_len).ok()?;
        let tp_snaplen = u32::try_from(snaplen).ok()?;
        // SAFETY: `base` names the selected frame inside this ring's live
        // block allocation; validated ring geometry keeps the complete frame
        // within that block. The ring Arc and lock keep the backing alive and
        // serialize kernel writers while userspace follows status ownership.
        let writer = unsafe { FrameWriter::new(base, self.config.frame_size) };

        match self.version {
            TpacketVersion::V1 => {
                let now_micros = crate::time::Instant::now().total_micros();
                let tp_sec = (now_micros / 1_000_000) as u32;
                let tp_usec = (now_micros % 1_000_000) as u32;
                // Clear the complete ABI object, including its four padding
                // bytes, so a recycled frame cannot disclose stale data.
                writer.zero(0, core::mem::size_of::<TpacketHdr>())?;
                writer.write(core::mem::offset_of!(TpacketHdr, tp_len), tp_len)?;
                writer.write(core::mem::offset_of!(TpacketHdr, tp_snaplen), tp_snaplen)?;
                writer.write(core::mem::offset_of!(TpacketHdr, tp_mac), tp_mac)?;
                writer.write(core::mem::offset_of!(TpacketHdr, tp_net), tp_net)?;
                writer.write(core::mem::offset_of!(TpacketHdr, tp_sec), tp_sec)?;
                writer.write(core::mem::offset_of!(TpacketHdr, tp_usec), tp_usec)?;
            }
            TpacketVersion::V2 => {
                let ts = crate::time::PosixTimeSpec::now();
                let tp_sec = ts.tv_sec as u32;
                let tp_nsec = ts.tv_nsec as u32;
                writer.zero(0, core::mem::size_of::<Tpacket2Hdr>())?;
                writer.write(core::mem::offset_of!(Tpacket2Hdr, tp_len), tp_len)?;
                writer.write(core::mem::offset_of!(Tpacket2Hdr, tp_snaplen), tp_snaplen)?;
                writer.write(core::mem::offset_of!(Tpacket2Hdr, tp_mac), tp_mac)?;
                writer.write(core::mem::offset_of!(Tpacket2Hdr, tp_net), tp_net)?;
                writer.write(core::mem::offset_of!(Tpacket2Hdr, tp_sec), tp_sec)?;
                writer.write(core::mem::offset_of!(Tpacket2Hdr, tp_nsec), tp_nsec)?;
                writer.write(
                    core::mem::offset_of!(Tpacket2Hdr, tp_vlan_tci),
                    meta.vlan_tci,
                )?;
                writer.write(
                    core::mem::offset_of!(Tpacket2Hdr, tp_vlan_tpid),
                    meta.vlan_tpid,
                )?;
            }
        }

        // sockaddr_ll follows the aligned header. hdrlen() already includes
        // the 20-byte sockaddr_ll, so the region starts at hdrlen - 20.
        let sll_off = self.version.hdrlen().checked_sub(20)?;
        writer.zero(sll_off, 20)?;
        writer.write(sll_off, 17u16)?; // sll_family = AF_PACKET
        writer.write(sll_off.checked_add(2)?, meta.protocol.to_be())?;
        writer.write(sll_off.checked_add(4)?, meta.ifindex as i32)?;
        writer.write(sll_off.checked_add(8)?, 1u16.to_be())?;
        writer.write(sll_off.checked_add(10)?, meta.pkt_type as u8)?;
        writer.write(sll_off.checked_add(11)?, 6u8)?;
        writer.copy_slice(sll_off.checked_add(12)?, &meta.src_mac)?;

        let (first, second) = input.visible_segments(snaplen)?;
        writer.copy_slice(data_off, first)?;
        writer.copy_slice(data_off.checked_add(first.len())?, second)?;

        // Compute final status: USER plus VLAN validity flags (V2 only).
        let mut status = TP_STATUS_USER;
        if is_vlan && self.version == TpacketVersion::V2 {
            status |= TP_STATUS_VLAN_VALID | TP_STATUS_VLAN_TPID_VALID;
        }
        Some(status)
    }
}

// ---------------------------------------------------------------------------
// Configuration validation — delegates to the standalone `tpacket` crate.
// ---------------------------------------------------------------------------

/// Validate a `tpacket_req` against the Linux rules and return the parsed config.
pub fn validate_ring_config(
    req: &super::uapi::TpacketReq,
    hdrlen: usize,
    reserve: usize,
) -> Result<RingConfig, SystemError> {
    tpacket::validate_ring_config(req, hdrlen, reserve, PAGE_SIZE).map_err(|_| SystemError::EINVAL)
}
