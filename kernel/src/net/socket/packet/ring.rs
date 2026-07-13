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

use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

use crate::arch::mm::LockedFrameAllocator;
use crate::arch::MMArch;
use crate::filesystem::page_cache::PageCache;
use crate::filesystem::vfs::{FileSystem, FsInfo, IndexNode, SuperBlock};
use crate::mm::allocator::page_frame::PageFrameCount;
use crate::mm::fault::{PageFaultHandler, PageFaultMessage};
use crate::mm::page::{page_manager_lock, PageFlags, PageType};
use crate::mm::MemoryManagementArch;
use crate::mm::VmFaultReason;

use super::uapi::{
    tpacket_align, TPACKET2_HDRLEN, TPACKET_HDRLEN, TP_STATUS_KERNEL, TP_STATUS_USER,
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
        // Ring-buffer pages are pre-allocated as PageType::Normal and inserted
        // into the page cache via insert_ready_page — they are NOT disk-backed
        // file pages, so the generic filemap_page_mkwrite (which requires
        // PageType::File, checks inode size, and prepares writeback) is both
        // unnecessary and harmful (it returns SIGBUS because the page type is
        // Normal, not File).
        //
        // Returning success here lets do_wp_page upgrade the PTE to read-write
        // in-place. The dirty-tracking block in do_wp_page is skipped for
        // Normal pages, which is correct — ring pages are never written back
        // to disk. This matches how the perf subsystem (PerfFakeFs) handles
        // its ring buffers.
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
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// TPACKET protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpacketVersion {
    V1,
    V2,
}

impl TpacketVersion {
    /// Header-region size per frame (aligned header + `sockaddr_ll`).
    pub fn hdrlen(&self) -> usize {
        match self {
            TpacketVersion::V1 => TPACKET_HDRLEN,
            TpacketVersion::V2 => TPACKET2_HDRLEN,
        }
    }
}

/// Parsed ring configuration.
#[derive(Debug, Clone, Copy)]
pub struct RingConfig {
    pub block_size: usize,
    pub block_nr: usize,
    pub frame_size: usize,
    pub frame_nr: usize,
}

/// Result of attempting to write a packet into the ring.
pub enum RingWriteResult {
    /// A frame was filled and published (status KERNEL→USER).
    Written,
    /// Every frame is still owned by userspace (TP_STATUS_USER); packet dropped.
    Dropped,
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
        let mut block_vaddrs = Vec::with_capacity(config.block_nr);

        // Per-block allocation (matches Linux alloc_pg_vec): each block is an
        // independent `block_size` contiguous physical run. This avoids one
        // large `block_nr * block_size` allocation that fails under fragmented
        // memory.
        let mut pm = page_manager_lock();
        for block_idx in 0..config.block_nr {
            let (phy_addr, pages) = pm.create_pages(
                PageType::Normal,
                PageFlags::PG_UNEVICTABLE,
                &mut LockedFrameAllocator,
                PageFrameCount::new(pages_per_block),
            )?;
            for j in 0..pages.len() {
                let page = pages.get(j).unwrap();
                page.write().add_flags(PageFlags::PG_UPTODATE);
                page_cache.insert_ready_page(block_idx * pages_per_block + j, page.clone())?;
            }
            let vaddr = unsafe { MMArch::phys_2_virt(phy_addr) }
                .ok_or(SystemError::EFAULT)?
                .data();
            // Zero this block. TP_STATUS_KERNEL == 0, so every frame in it
            // starts KERNEL-owned and is immediately writable.
            unsafe { core::ptr::write_bytes(vaddr as *mut u8, 0, config.block_size) };
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

    /// Returns `true` if at least one frame is in `TP_STATUS_USER` (readable by
    /// userspace). Used by `can_recv()` / poll readiness.
    ///
    /// # 性能说明
    ///
    /// 这是 O(frame_nr) 线性扫描，每个帧做一次 `tp_status` 原子读。它仅在
    /// poll/epoll readiness 检查路径（`can_recv` → `has_user_frames`）上调用，
    /// 不在数据收发的热路径上，因此不会逐包执行。
    ///
    /// 对于典型 ring（frame_nr = 1024），每次 poll 最多扫描 1024 次原子读；早
    /// 命中即返回，平均成本远低于最坏情况。该开销在 poll 路径上可接受。
    ///
    /// # 未来优化方向（当前不实现）
    ///
    /// 若未来需要在超大 ring 或高 poll 频率场景下进一步降低成本，可维护一个
    /// `AtomicU32 user_frame_count` 计数器：`write_frame` 发布 KERNEL→USER 时
    /// 自增，从而把扫描降到 O(1)。但用户态把 `tp_status` 翻回 `TP_STATUS_KERNEL`
    /// 时无法主动通知内核，计数无法准确递减，必须在 `has_user_frames` 中走懒
    /// 更新（扫描确认）或要求用户态显式 `recv`/`poll` 来同步状态。这会让计数方案
    /// 的复杂度显著高于当前的简单扫描，故暂不引入。
    pub fn has_user_frames(&self) -> bool {
        for i in 0..self.config.frame_nr {
            let base = self.frame_base(i);
            if self.read_tp_status(base) == TP_STATUS_USER {
                return true;
            }
        }
        false
    }

    /// Write one packet into the ring. Caller must hold the ring lock.
    pub fn write_frame(
        &mut self,
        frame: &[u8],
        meta: &PacketMetadata,
    ) -> RingWriteResult {
        let hdrlen = self.version.hdrlen();
        let is_vlan = meta.vlan_tpid != 0;
        let mac_len = if is_vlan { 18 } else { 14 };
        // Linux formula: netoff = TPACKET_ALIGN(hdrlen + max(maclen, 16)) + reserve.
        // This guarantees tp_net is 16-byte aligned. For SOCK_RAW the MAC
        // header lives at tp_mac = netoff - mac_len, so the visible data
        // (including the MAC) starts at tp_mac. For SOCK_DGRAM there is no
        // MAC, so tp_mac == tp_net == netoff and data starts at netoff.
        let netoff = tpacket_align(hdrlen + core::cmp::max(mac_len, 16)) + self.reserve;
        let data_off = if self.raw { netoff - mac_len } else { netoff };
        let data_cap = self.config.frame_size.saturating_sub(data_off);
        if data_cap == 0 {
            return RingWriteResult::Dropped;
        }

        // Find the next KERNEL-owned frame, scanning from `head`.
        let mut found = None;
        for i in 0..self.config.frame_nr {
            let idx = ((self.head as usize + i) % self.config.frame_nr) as u32;
            let base = self.frame_base(idx as usize);
            if self.read_tp_status(base) == TP_STATUS_KERNEL {
                found = Some((idx, base));
                break;
            }
        }
        let (idx, base) = match found {
            Some(x) => x,
            None => return RingWriteResult::Dropped,
        };

        let status = self.fill_frame(base, frame, meta, netoff, data_off, data_cap);

        // Publish: flip status KERNEL→USER *last*, with Release ordering so the
        // data writes above are visible before userspace observes USER.
        self.publish(base, status);

        self.head = ((idx as usize + 1) % self.config.frame_nr) as u32;
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

    /// Fill the header and copy packet data into the frame at `base`.
    /// VLAN tags are stripped from the data region (matching the queue path in
    /// `rx.rs`) while VLAN metadata is recorded in the V2 header.
    fn fill_frame(
        &self,
        base: usize,
        frame: &[u8],
        meta: &PacketMetadata,
        netoff: usize,
        data_off: usize,
        data_cap: usize,
    ) -> u32 {
        let is_vlan = meta.vlan_tpid != 0;

        // tp_net = netoff (guaranteed 16-byte aligned by the formula in
        // write_frame).  tp_mac = the byte offset where visible data starts.
        //   RAW:  tp_mac = netoff - mac_len (MAC header at tp_mac).
        //   DGRAM: tp_mac == tp_net == netoff (no MAC in the frame).
        let tp_mac = data_off as u16;
        let tp_net = netoff as u16;

        // Compute snaplen and wire length from the *visible* (VLAN-stripped) length.
        let visible_len = if is_vlan {
            frame.len().saturating_sub(4)
        } else {
            frame.len()
        };
        let wire_len = if self.raw {
            visible_len
        } else {
            visible_len.saturating_sub(14)
        };
        let snaplen = wire_len.min(data_cap);

        // Timestamps are taken from per-version sources to match Linux
        // semantics:
        //   V1: microsecond resolution (struct timeval).
        //   V2: nanosecond resolution (struct timespec), so we must read the
        //       real nanoseconds from PosixTimeSpec rather than scaling a
        //       microsecond value.
        let dst = base as *mut u8;

        unsafe {
            match self.version {
                TpacketVersion::V1 => {
                    let now_micros = crate::time::Instant::now().total_micros();
                    let tp_sec = (now_micros / 1_000_000) as u32;
                    let tp_usec = (now_micros % 1_000_000) as u32;
                    // tp_status written last via publish(); zero for now.
                    *(dst.add(0) as *mut u64) = 0; // tp_status (placeholder)
                    *(dst.add(8) as *mut u32) = wire_len as u32; // tp_len
                    *(dst.add(12) as *mut u32) = snaplen as u32; // tp_snaplen
                    *(dst.add(16) as *mut u16) = tp_mac;
                    *(dst.add(18) as *mut u16) = tp_net;
                    *(dst.add(20) as *mut u32) = tp_sec;
                    *(dst.add(24) as *mut u32) = tp_usec;
                }
                TpacketVersion::V2 => {
                    let ts = crate::time::PosixTimeSpec::now();
                    let tp_sec = ts.tv_sec as u32;
                    let tp_nsec = ts.tv_nsec as u32;
                    *(dst.add(0) as *mut u32) = 0; // tp_status (placeholder)
                    *(dst.add(4) as *mut u32) = wire_len as u32; // tp_len
                    *(dst.add(8) as *mut u32) = snaplen as u32; // tp_snaplen
                    *(dst.add(12) as *mut u16) = tp_mac;
                    *(dst.add(14) as *mut u16) = tp_net;
                    *(dst.add(16) as *mut u32) = tp_sec;
                    *(dst.add(20) as *mut u32) = tp_nsec; // tp_nsec
                    *(dst.add(24) as *mut u16) = meta.vlan_tci; // tp_vlan_tci
                    *(dst.add(26) as *mut u16) = meta.vlan_tpid; // tp_vlan_tpid
                    // tp_padding stays zero
                }
            }

            // Fill the sockaddr_ll region that follows the aligned header.
            // sll_offset = TPACKET_ALIGN(sizeof(hdr)) for both V1 (align(28)=32)
            // and V2 (align(32)=32).  20 bytes total.
            let sll_off = tpacket_align(match self.version {
                TpacketVersion::V1 => 28,
                TpacketVersion::V2 => 32,
            });
            *(dst.add(sll_off) as *mut u16) = 17u16; // sll_family = AF_PACKET
            *(dst.add(sll_off + 2) as *mut u16) = meta.protocol.to_be(); // sll_protocol
            *(dst.add(sll_off + 4) as *mut i32) = meta.ifindex as i32; // sll_ifindex
            *(dst.add(sll_off + 8) as *mut u16) = 1u16.to_be(); // sll_hatype = ARPHRD_ETHER
            *(dst.add(sll_off + 10) as *mut u8) = meta.pkt_type as u8; // sll_pkttype
            *(dst.add(sll_off + 11) as *mut u8) = 6; // sll_halen
            core::ptr::copy_nonoverlapping(
                meta.src_mac.as_ptr(),
                dst.add(sll_off + 12),
                6,
            );

            // Copy packet data into the data region (starting at data_off).
            let data_dst = dst.add(data_off);
            if self.raw {
                if is_vlan {
                    // Strip the 4-byte VLAN tag: [0..12] + [16..]
                    let pre = 12usize;
                    let copy_len = snaplen.min(pre);
                    core::ptr::copy_nonoverlapping(frame.as_ptr(), data_dst, copy_len);
                    let remain = snaplen.saturating_sub(pre);
                    if remain > 0 {
                        let src_off = 16usize;
                        let n = remain.min(frame.len().saturating_sub(src_off));
                        core::ptr::copy_nonoverlapping(
                            frame.as_ptr().add(src_off),
                            data_dst.add(pre),
                            n,
                        );
                    }
                } else {
                    let n = snaplen.min(frame.len());
                    core::ptr::copy_nonoverlapping(frame.as_ptr(), data_dst, n);
                }
            } else {
                // DGRAM: skip MAC header
                let start = if is_vlan { 18 } else { 14 };
                let n = snaplen.min(frame.len().saturating_sub(start));
                core::ptr::copy_nonoverlapping(frame.as_ptr().add(start), data_dst, n);
            }
        }

        // Compute final status: USER plus VLAN validity flags (V2 only).
        let mut status = TP_STATUS_USER;
        if is_vlan && self.version == TpacketVersion::V2 {
            status |= TP_STATUS_VLAN_VALID | TP_STATUS_VLAN_TPID_VALID;
        }
        status
    }
}

// ---------------------------------------------------------------------------
// Configuration validation (§2.5 rules from Linux packet_setring)
// ---------------------------------------------------------------------------

/// Validate a `tpacket_req` against the Linux rules and return the parsed config.
pub fn validate_ring_config(
    req: &super::uapi::TpacketReq,
    hdrlen: usize,
    reserve: usize,
) -> Result<RingConfig, SystemError> {
    let block_size = req.tp_block_size as usize;
    let block_nr = req.tp_block_nr as usize;
    let frame_size = req.tp_frame_size as usize;
    let frame_nr = req.tp_frame_nr as usize;

    // block_size > 0, page-aligned.
    if block_size == 0 || block_size % PAGE_SIZE != 0 {
        return Err(SystemError::EINVAL);
    }
    // Overflow guard: ensure block_nr * block_size does not overflow and is
    // non-zero. Done early (before frame_size checks) because subsequent
    // validation and the eventual allocation depend on the total ring size.
    let total = block_nr.checked_mul(block_size).ok_or(SystemError::EINVAL)?;
    if total == 0 {
        return Err(SystemError::EINVAL);
    }
    // frame_size >= hdrlen + reserve, and 16-byte aligned.
    let min_frame_size = hdrlen + reserve;
    if frame_size < min_frame_size || frame_size % tpacket_align(1) != 0 {
        return Err(SystemError::EINVAL);
    }
    // frames_per_block > 0.
    if frame_size > block_size {
        return Err(SystemError::EINVAL);
    }
    let frames_per_block = block_size / frame_size;
    if frames_per_block == 0 {
        return Err(SystemError::EINVAL);
    }
    // frame_nr consistency: frames_per_block * block_nr == frame_nr.
    if frames_per_block.checked_mul(block_nr) != Some(frame_nr) {
        return Err(SystemError::EINVAL);
    }

    Ok(RingConfig {
        block_size,
        block_nr,
        frame_size,
        frame_nr,
    })
}
