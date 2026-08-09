//! TPACKET ring buffer UAPI types and configuration validation.
//!
//! Pure-data definitions matching `include/uapi/linux/if_packet.h` (Linux 6.6),
//! plus the ring-configuration validation rules from Linux `packet_setring()`.
//!
//! Extracted into a standalone crate so that `#[repr(C)]` layout assertions
//! and configuration edge cases can be unit-tested on the host without booting
//! the kernel.

#![no_std]

use core::fmt;

// ---------------------------------------------------------------------------
// TPACKET versions
// ---------------------------------------------------------------------------

/// TPACKET ring versions (enum tpacket_versions).
pub mod tpacket_version {
    pub const TPACKET_V1: i32 = 0;
    pub const TPACKET_V2: i32 = 1;
    pub const TPACKET_V3: i32 = 2;
}

/// TPACKET protocol version.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TpacketVersion {
    V1,
    V2,
}

impl TpacketVersion {
    /// Raw header struct size (before alignment / sockaddr_ll).
    pub const fn raw_hdr_size(&self) -> usize {
        match self {
            TpacketVersion::V1 => 28,
            TpacketVersion::V2 => 32,
        }
    }

    /// Header-region size per frame (aligned header + `sockaddr_ll` = 20 bytes).
    /// V1: align(28)+20 = 48.  V2: align(32)+20 = 52.
    pub const fn hdrlen(&self) -> usize {
        tpacket_align(Self::raw_hdr_size_size_of(self)) + SOCKADDR_LL_SIZE
    }

    /// `size_of::<TpacketHdr>()` / `size_of::<Tpacket2Hdr>()` equivalent.
    const fn raw_hdr_size_size_of(v: &Self) -> usize {
        match v {
            TpacketVersion::V1 => 28,
            TpacketVersion::V2 => 32,
        }
    }
}

// ---------------------------------------------------------------------------
// Alignment
// ---------------------------------------------------------------------------

/// Frame alignment for all TPACKET versions.
pub const TPACKET_ALIGNMENT: usize = 16;

/// Align `x` up to [`TPACKET_ALIGNMENT`].
pub const fn tpacket_align(x: usize) -> usize {
    (x + TPACKET_ALIGNMENT - 1) & !(TPACKET_ALIGNMENT - 1)
}

/// `struct sockaddr_ll` is 20 bytes on all supported architectures.
const SOCKADDR_LL_SIZE: usize = 20;

/// Header region size per version (= `tpacket_align(sizeof(hdr)) + sockaddr_ll`).
pub const TPACKET_HDRLEN: usize = tpacket_align(28) + SOCKADDR_LL_SIZE;
pub const TPACKET2_HDRLEN: usize = tpacket_align(32) + SOCKADDR_LL_SIZE;

// ---------------------------------------------------------------------------
// RX ring header status flags
// ---------------------------------------------------------------------------

pub const TP_STATUS_KERNEL: u32 = 0;
pub const TP_STATUS_USER: u32 = 1;
pub const TP_STATUS_LOSING: u32 = 1 << 2;
pub const TP_STATUS_VLAN_VALID: u32 = 1 << 4;
pub const TP_STATUS_VLAN_TPID_VALID: u32 = 1 << 6;

// ---------------------------------------------------------------------------
// `#[repr(C)]` header structures
// ---------------------------------------------------------------------------

/// V1 frame header (`struct tpacket_hdr`). 28 bytes on x86_64.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TpacketHdr {
    pub tp_status: u64,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_sec: u32,
    pub tp_usec: u32,
}

/// V2 frame header (`struct tpacket2_hdr`). 32 bytes.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Tpacket2Hdr {
    pub tp_status: u32,
    pub tp_len: u32,
    pub tp_snaplen: u32,
    pub tp_mac: u16,
    pub tp_net: u16,
    pub tp_sec: u32,
    pub tp_nsec: u32,
    pub tp_vlan_tci: u16,
    pub tp_vlan_tpid: u16,
    pub tp_padding: [u8; 4],
}

/// `struct tpacket_req` — ring configuration for V1/V2.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TpacketReq {
    pub tp_block_size: u32,
    pub tp_block_nr: u32,
    pub tp_frame_size: u32,
    pub tp_frame_nr: u32,
}

/// `struct tpacket_stats` — returned by PACKET_STATISTICS (V1/V2).
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct TpacketStats {
    pub tp_packets: u32,
    pub tp_drops: u32,
}

// ---------------------------------------------------------------------------
// Ring configuration validation
// ---------------------------------------------------------------------------

/// Parsed ring configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RingConfig {
    pub block_size: usize,
    pub block_nr: usize,
    pub frame_size: usize,
    pub frame_nr: usize,
}

/// Error returned by [`validate_ring_config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingConfigError {
    /// block_size == 0 or not page-aligned.
    InvalidBlockSize,
    /// block_nr * block_size overflows or is zero.
    InvalidTotalSize,
    /// frame_size < hdrlen + reserve, or not 16-byte aligned.
    InvalidFrameSize,
    /// frame_size > block_size (frames_per_block would be 0).
    FrameLargerThanBlock,
    /// frames_per_block * block_nr != frame_nr.
    FrameNrMismatch,
}

impl fmt::Display for RingConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RingConfigError::InvalidBlockSize => write!(f, "block_size is 0 or not page-aligned"),
            RingConfigError::InvalidTotalSize => write!(f, "block_nr * block_size overflows or is 0"),
            RingConfigError::InvalidFrameSize => {
                write!(f, "frame_size < hdrlen + reserve or not 16-byte aligned")
            }
            RingConfigError::FrameLargerThanBlock => write!(f, "frame_size > block_size"),
            RingConfigError::FrameNrMismatch => {
                write!(f, "frames_per_block * block_nr != frame_nr")
            }
        }
    }
}

/// Default page size used for validation when the crate is consumed outside
/// the kernel (tests).  The kernel caller passes the architecture page size.
pub const DEFAULT_PAGE_SIZE: usize = 4096;

/// Validate a `tpacket_req` against the Linux `packet_setring` rules.
///
/// Mirrors the validation in Linux `packet_set_ring()`:
/// 1. `block_size` must be non-zero and a multiple of the page size.
/// 2. `block_nr * block_size` must not overflow and must be non-zero.
/// 3. `frame_size` must be >= `hdrlen + reserve` and 16-byte aligned.
/// 4. `frame_size` must be <= `block_size`.
/// 5. `frames_per_block * block_nr` must equal `frame_nr`.
pub fn validate_ring_config(
    req: &TpacketReq,
    hdrlen: usize,
    reserve: usize,
    page_size: usize,
) -> Result<RingConfig, RingConfigError> {
    let block_size = req.tp_block_size as usize;
    let block_nr = req.tp_block_nr as usize;
    let frame_size = req.tp_frame_size as usize;
    let frame_nr = req.tp_frame_nr as usize;

    if block_size == 0 || !block_size.is_multiple_of(page_size) {
        return Err(RingConfigError::InvalidBlockSize);
    }

    let total = block_nr
        .checked_mul(block_size)
        .ok_or(RingConfigError::InvalidTotalSize)?;
    if total == 0 {
        return Err(RingConfigError::InvalidTotalSize);
    }

    let min_frame_size = hdrlen + reserve;
    if frame_size < min_frame_size || !frame_size.is_multiple_of(tpacket_align(1)) {
        return Err(RingConfigError::InvalidFrameSize);
    }

    if frame_size > block_size {
        return Err(RingConfigError::FrameLargerThanBlock);
    }

    let frames_per_block = block_size / frame_size;
    if frames_per_block.checked_mul(block_nr) != Some(frame_nr) {
        return Err(RingConfigError::FrameNrMismatch);
    }

    Ok(RingConfig {
        block_size,
        block_nr,
        frame_size,
        frame_nr,
    })
}

#[cfg(test)]
mod tests;
