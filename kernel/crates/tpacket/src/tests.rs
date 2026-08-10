use crate::*;

// ===========================================================================
// ABI layout assertions — these MUST match Linux exactly for mmap compat.
// ===========================================================================

#[test]
fn tpacket_hdr_v1_size() {
    // sizeof(struct tpacket_hdr) = 32 on x86_64 (u64 tp_status forces 8-byte
    // alignment, struct padded to 32). Matches Linux kernel UAPI.
    assert_eq!(core::mem::size_of::<TpacketHdr>(), 32);
}

#[test]
fn tpacket_hdr_v2_size() {
    assert_eq!(core::mem::size_of::<Tpacket2Hdr>(), 32);
}

#[test]
fn tpacket_req_size() {
    assert_eq!(core::mem::size_of::<TpacketReq>(), 16);
}

#[test]
fn tpacket_stats_size() {
    assert_eq!(core::mem::size_of::<TpacketStats>(), 8);
}

#[test]
fn hdrlen_v1_is_aligned_plus_sockaddr_ll() {
    // V1: align(sizeof(hdr)=32) + 20 = 52
    assert_eq!(TPACKET_HDRLEN, tpacket_align(32) + 20);
    assert_eq!(TpacketVersion::V1.hdrlen(), TPACKET_HDRLEN);
}

#[test]
fn hdrlen_v2_is_aligned_plus_sockaddr_ll() {
    // V2: align(sizeof(hdr)=32) + 20 = 52
    assert_eq!(TPACKET2_HDRLEN, tpacket_align(32) + 20);
    assert_eq!(TpacketVersion::V2.hdrlen(), TPACKET2_HDRLEN);
}

#[test]
fn tpacket_align_values() {
    assert_eq!(tpacket_align(0), 0);
    assert_eq!(tpacket_align(1), 16);
    assert_eq!(tpacket_align(15), 16);
    assert_eq!(tpacket_align(16), 16);
    assert_eq!(tpacket_align(17), 32);
    assert_eq!(tpacket_align(28), 32);
    assert_eq!(tpacket_align(32), 32);
    assert_eq!(tpacket_align(52), 64);
}

#[test]
fn raw_and_dgram_offsets_follow_linux_formulas() {
    assert_eq!(
        calculate_frame_offsets(TPACKET_HDRLEN, 14, 0, true),
        Some(FrameOffsets {
            macoff: 66,
            netoff: 80,
        })
    );
    assert_eq!(
        calculate_frame_offsets(TPACKET_HDRLEN, 14, 0, false),
        Some(FrameOffsets {
            macoff: 80,
            netoff: 80,
        })
    );
}

#[test]
fn offsets_accept_u16_boundary_and_reject_truncation() {
    let aligned_hdr = tpacket_align(TPACKET_HDRLEN);
    let reserve_at_max = u16::MAX as usize - aligned_hdr - 16;
    assert_eq!(
        calculate_frame_offsets(TPACKET_HDRLEN, 14, reserve_at_max, false),
        Some(FrameOffsets {
            macoff: u16::MAX,
            netoff: u16::MAX,
        })
    );
    assert_eq!(
        calculate_frame_offsets(TPACKET_HDRLEN, 14, reserve_at_max + 1, false),
        None
    );
}

#[test]
fn offsets_reject_checked_arithmetic_overflow() {
    assert_eq!(calculate_frame_offsets(usize::MAX, 14, 0, false), None);
    assert_eq!(calculate_frame_offsets(64, usize::MAX, 0, true), None);
}

/// Minimum valid frame_size = tpacket_align(TPACKET_HDRLEN) = 64.
/// frame_size must be >= hdrlen AND 16-byte aligned.
const MIN_FRAME_SIZE: usize = 64;

// ===========================================================================
// validate_ring_config — Linux packet_setring rules
// ===========================================================================

#[test]
fn valid_config() {
    let fpb = 4096 / MIN_FRAME_SIZE; // 64
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 1,
        tp_frame_size: MIN_FRAME_SIZE as u32,
        tp_frame_nr: fpb as u32,
    };
    let cfg = validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE).unwrap();
    assert_eq!(cfg.block_size, 4096);
    assert_eq!(cfg.block_nr, 1);
    assert_eq!(cfg.frame_size, MIN_FRAME_SIZE);
    assert_eq!(cfg.frame_nr, fpb);
}

#[test]
fn reject_zero_block_size() {
    let req = TpacketReq {
        tp_block_size: 0,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidBlockSize)
    );
}

#[test]
fn reject_non_page_aligned_block_size() {
    let req = TpacketReq {
        tp_block_size: 100,
        tp_block_nr: 1,
        tp_frame_size: MIN_FRAME_SIZE as u32,
        tp_frame_nr: 1,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidBlockSize)
    );
}

#[test]
fn reject_block_nr_zero() {
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 0,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidTotalSize)
    );
}

#[test]
fn reject_frame_too_small() {
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 1,
        tp_frame_size: 16, // < hdrlen (52)
        tp_frame_nr: 256,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidFrameSize)
    );
}

#[test]
fn reject_frame_not_16_aligned() {
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 1,
        tp_frame_size: 52, // == hdrlen but not 16-aligned (52 % 16 = 4)
        tp_frame_nr: 78,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidFrameSize)
    );
}

#[test]
fn reject_frame_larger_than_block() {
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 1,
        tp_frame_size: 8192,
        tp_frame_nr: 1,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::FrameLargerThanBlock)
    );
}

#[test]
fn reject_frame_nr_mismatch() {
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 2,
        tp_frame_size: 4096,
        tp_frame_nr: 1, // should be 2
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::FrameNrMismatch)
    );
}

#[test]
fn valid_config_with_reserve() {
    let reserve = 32;
    let min_frame = tpacket_align(TPACKET_HDRLEN + reserve); // align(84) = 96
    let fpb = 4096 / min_frame;
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 1,
        tp_frame_size: min_frame as u32,
        tp_frame_nr: fpb as u32,
        ..Default::default()
    };
    let cfg = validate_ring_config(&req, TPACKET_HDRLEN, reserve, DEFAULT_PAGE_SIZE).unwrap();
    assert_eq!(cfg.frame_size, min_frame);
}

#[test]
fn reject_frame_too_small_with_reserve() {
    // frame_size >= hdrlen and 16-aligned, but < hdrlen + reserve
    let req = TpacketReq {
        tp_block_size: 4096,
        tp_block_nr: 1,
        tp_frame_size: MIN_FRAME_SIZE as u32, // 64, but hdrlen+reserve=84
        tp_frame_nr: 64,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 32, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidFrameSize)
    );
}

#[test]
fn multi_block_config() {
    let block_size = 4096u32;
    let frame_size = MIN_FRAME_SIZE as u32; // 64
    let frames_per_block = block_size / frame_size; // 64
    let block_nr = 4u32;
    let req = TpacketReq {
        tp_block_size: block_size,
        tp_block_nr: block_nr,
        tp_frame_size: frame_size,
        tp_frame_nr: frames_per_block * block_nr,
    };
    let cfg = validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE).unwrap();
    assert_eq!(cfg.block_nr, 4);
    assert_eq!(cfg.frame_nr, (frames_per_block * block_nr) as usize);
}

#[test]
fn v2_valid_config() {
    let block_size = 4096u32;
    let frame_size = tpacket_align(TPACKET2_HDRLEN) as u32; // align(52) = 64
    let frames_per_block = block_size / frame_size; // 64
    let block_nr = 2u32;
    let req = TpacketReq {
        tp_block_size: block_size,
        tp_block_nr: block_nr,
        tp_frame_size: frame_size,
        tp_frame_nr: frames_per_block * block_nr,
    };
    let cfg = validate_ring_config(&req, TPACKET2_HDRLEN, 0, DEFAULT_PAGE_SIZE).unwrap();
    assert_eq!(cfg.frame_size, frame_size as usize);
    assert_eq!(cfg.frame_nr, (frames_per_block * block_nr) as usize);
}

#[test]
fn overflow_guard() {
    // block_size = u32::MAX is not page-aligned, so it fails block_size check first
    let req = TpacketReq {
        tp_block_size: u32::MAX,
        tp_block_nr: u32::MAX,
        tp_frame_size: MIN_FRAME_SIZE as u32,
        tp_frame_nr: 1,
        ..Default::default()
    };
    assert_eq!(
        validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE),
        Err(RingConfigError::InvalidBlockSize)
    );
}

#[test]
fn overflow_guard_total() {
    // block_size is valid (page-aligned) but block_nr * block_size overflows
    // On 64-bit: 0x40000000 * 0x40000000 = 2^62 which fits in usize
    // Use values that actually overflow on 64-bit: need product > 2^64
    // u32 * u32 max = (2^32-1)^2 ≈ 2^64, doesn't overflow usize on 64-bit
    // So on 64-bit this can't happen with u32 inputs. Test with large page-aligned values.
    // block_nr=0x80000000, block_size=0x200000 (2 MiB, page-aligned)
    // product = 0x80000000 * 0x200000 = 2^31 * 2^21 = 2^52 — fits in usize
    // This test just verifies a large valid config works on 64-bit.
    let req = TpacketReq {
        tp_block_size: 0x200000, // 2 MiB
        tp_block_nr: 0x100,      // 256 blocks = 512 MiB total
        tp_frame_size: MIN_FRAME_SIZE as u32,
        tp_frame_nr: (0x200000 / MIN_FRAME_SIZE as u32 * 0x100),
        ..Default::default()
    };
    let cfg = validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE).unwrap();
    assert_eq!(cfg.block_size, 0x200000);
}

#[test]
fn non_power_of_two_block_size_valid_if_page_aligned() {
    // 12 KiB block (3 pages) — the case from review comment 7
    let block_size = 4096 * 3; // 12288
    let frame_size = MIN_FRAME_SIZE; // 64
    let frames_per_block = block_size / frame_size; // 192
    let block_nr = 2;
    let req = TpacketReq {
        tp_block_size: block_size as u32,
        tp_block_nr: block_nr as u32,
        tp_frame_size: frame_size as u32,
        tp_frame_nr: (frames_per_block * block_nr) as u32,
        ..Default::default()
    };
    let cfg = validate_ring_config(&req, TPACKET_HDRLEN, 0, DEFAULT_PAGE_SIZE).unwrap();
    assert_eq!(cfg.block_size, block_size);
}

#[test]
fn hdrlen_v1_equals_v2() {
    // Both V1 and V2 headers are 32 bytes, so hdrlen is the same.
    assert_eq!(TPACKET_HDRLEN, TPACKET2_HDRLEN);
    assert_eq!(TpacketVersion::V1.hdrlen(), TpacketVersion::V2.hdrlen());
}
