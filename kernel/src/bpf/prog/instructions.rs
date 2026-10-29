//! Byte-level layout rules for the eBPF instruction stream.
//!
//! `BPF_PROG_LOAD` hands the kernel a raw byte stream, so the loader is the
//! first place that has to know how wide an instruction is: every instruction
//! occupies one eight-byte slot except `BPF_LD | BPF_IMM | BPF_DW`, whose
//! 64-bit immediate spans two. These rules mirror the layout half of Linux
//! `resolve_pseudo_ldimm64()`; keeping them here rather than inline in the
//! relocation loop gives the encoding one owner and lets it be unit tested
//! without building a whole program.

use crate::include::bindings::linux_bpf::{
    BPF_PSEUDO_MAP_FD, BPF_PSEUDO_MAP_IDX, BPF_PSEUDO_MAP_IDX_VALUE, BPF_PSEUDO_MAP_VALUE,
};
use system_error::SystemError;

/// One BPF instruction slot is eight bytes wide.
pub(super) const INSN_SIZE: usize = rbpf::ebpf::INSN_SIZE;

/// `BPF_LD | BPF_IMM | BPF_DW`: the only instruction wider than one slot.
const LD_DW_IMM_OPC: u8 = rbpf::ebpf::LD_DW_IMM;

/// `BPF_CLASS(code)`: the low three bits of an opcode.
const BPF_CLS_MASK: u8 = 0x07;
/// `BPF_MODE(code)`: the addressing mode bits of a load/store opcode.
const BPF_MODE_MASK: u8 = 0xe0;
/// `BPF_LDX` register-based load instructions.
const BPF_LDX: u8 = 0x01;
/// `BPF_MODE` values `LDX` is allowed to use (`BPF_MEM`/`BPF_MEMSX`).
const BPF_MEM: u8 = 0x60;
const BPF_MEMSX: u8 = 0x80;

/// The 64-bit immediate of an `LD_DW_IMM`, classified by its `src_reg`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum LdDwImm {
    /// `src_reg == 0`: a plain constant that needs no relocation.
    Imm64,
    /// `BPF_PSEUDO_MAP_FD`: the immediate is a map file descriptor.
    MapFd { fd: u32 },
    /// `BPF_PSEUDO_MAP_VALUE`: the immediate is a map descriptor and the
    /// second slot holds the byte offset of the value the instruction loads.
    MapValue { fd: u32, offset: u32 },
}

/// Validate the instruction slot at `index` against the layout rules Linux
/// applies in `resolve_pseudo_ldimm64()`, and report whether it begins a
/// two-slot `LD_DW_IMM` (in which case the caller must advance two slots).
///
/// The caller keeps `index < insns.len() / INSN_SIZE`.
pub(super) fn check_slot(insns: &[u8], index: usize) -> Result<bool, SystemError> {
    let slot = &insns[index * INSN_SIZE..(index + 1) * INSN_SIZE];
    let code = slot[0];
    let imm = i32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]);

    // `BPF_LDX` may only use the `MEM`/`MEMSX` modes and must leave `imm` zero.
    let mode = code & BPF_MODE_MASK;
    if code & BPF_CLS_MASK == BPF_LDX && ((mode != BPF_MEM && mode != BPF_MEMSX) || imm != 0) {
        return Err(SystemError::EINVAL);
    }

    if code == LD_DW_IMM_OPC {
        return Ok(true);
    }
    // Opcodes that pass the layout checks above but are not in Linux's
    // instruction table are rejected here, before a program could be
    // published for them.
    if !is_known_opcode(code) {
        return Err(SystemError::EINVAL);
    }
    Ok(false)
}

/// Validate and classify the `LD_DW_IMM` whose first slot is at `index`.
///
/// Mirrors Linux `resolve_pseudo_ldimm64()` and the `check_ld_imm()` rule it
/// relies on: neither slot may put anything into the fields that are reserved,
/// `MAP_FD` additionally requires the high immediate to be zero, and
/// pseudo-register classes that cannot be resolved are reported as errors
/// instead of being passed through as constants.
pub(super) fn decode_ld_dw_imm(insns: &[u8], index: usize) -> Result<LdDwImm, SystemError> {
    let slot = insns
        .get(index * INSN_SIZE..(index + 1) * INSN_SIZE)
        .ok_or(SystemError::EINVAL)?;
    let next = insns
        .get((index + 1) * INSN_SIZE..(index + 2) * INSN_SIZE)
        .ok_or(SystemError::EINVAL)?;
    // The `code`, `dst_reg`, `src_reg` and `off` fields of the second slot are
    // reserved and must be zero.
    if next[0] != 0 || next[1] != 0 || next[2] != 0 || next[3] != 0 {
        return Err(SystemError::EINVAL);
    }
    // Linux `check_ld_imm()`: a 64-bit immediate load has no branch offset.
    if slot[2] != 0 || slot[3] != 0 {
        return Err(SystemError::EINVAL);
    }

    let fd = u32::from_le_bytes([slot[4], slot[5], slot[6], slot[7]]);
    let offset = u32::from_le_bytes([next[4], next[5], next[6], next[7]]);
    match u32::from(slot[1] >> 4) {
        0 => Ok(LdDwImm::Imm64),
        BPF_PSEUDO_MAP_FD => {
            if offset != 0 {
                return Err(SystemError::EINVAL);
            }
            Ok(LdDwImm::MapFd { fd })
        }
        BPF_PSEUDO_MAP_VALUE => Ok(LdDwImm::MapValue { fd, offset }),
        // `BPF_PSEUDO_MAP_IDX*` selects a descriptor out of the `fd_array`
        // that a plain `BPF_PROG_LOAD` never supplies. Linux reports `EPROTO`
        // for that condition; keep the same errno rather than pretending the
        // descriptor could be resolved locally.
        BPF_PSEUDO_MAP_IDX => {
            if offset != 0 {
                return Err(SystemError::EINVAL);
            }
            Err(SystemError::EPROTO)
        }
        BPF_PSEUDO_MAP_IDX_VALUE => Err(SystemError::EPROTO),
        // BTF ids, subprograms and unknown classes need infrastructure this
        // kernel does not implement (or are not pseudo loads at all). Reject
        // them instead of misreading the immediate as a constant.
        _ => Err(SystemError::EINVAL),
    }
}

/// Whether `opcode` is one that Linux `bpf_opcode_in_insntable()` accepts.
///
/// Taken from `BPF_INSN_MAP` in Linux 6.6 `kernel/bpf/core.c`.
fn is_known_opcode(opcode: u8) -> bool {
    INSN_TABLE.binary_search(&opcode).is_ok()
}

/// Store a 64-bit address in the immediate fields of two consecutive slots.
///
/// The caller must have validated the slots with [`decode_ld_dw_imm`].
pub(super) fn write_imm64(insns: &mut [u8], index: usize, value: u64) {
    let low = (value as u32).to_le_bytes();
    let high = ((value >> 32) as u32).to_le_bytes();
    insns[index * INSN_SIZE + 4..(index + 1) * INSN_SIZE].copy_from_slice(&low);
    insns[(index + 1) * INSN_SIZE + 4..(index + 2) * INSN_SIZE].copy_from_slice(&high);
}

/// Sorted opcodes of Linux `BPF_INSN_MAP` (`kernel/bpf/core.c`) plus the six
/// cBPF load forms the UAPI still exposes, i.e. exactly the set for which
/// `bpf_opcode_in_insntable()` returns true.
const INSN_TABLE: [u8; 125] = [
    0x04, 0x05, 0x06, 0x07, 0x0c, 0x0f, 0x14, 0x15, 0x16, 0x17, 0x18, 0x1c, 0x1d, 0x1e, 0x1f, 0x20,
    0x24, 0x25, 0x26, 0x27, 0x28, 0x2c, 0x2d, 0x2e, 0x2f, 0x30, 0x34, 0x35, 0x36, 0x37, 0x3c, 0x3d,
    0x3e, 0x3f, 0x40, 0x44, 0x45, 0x46, 0x47, 0x48, 0x4c, 0x4d, 0x4e, 0x4f, 0x50, 0x54, 0x55, 0x56,
    0x57, 0x5c, 0x5d, 0x5e, 0x5f, 0x61, 0x62, 0x63, 0x64, 0x65, 0x66, 0x67, 0x69, 0x6a, 0x6b, 0x6c,
    0x6d, 0x6e, 0x6f, 0x71, 0x72, 0x73, 0x74, 0x75, 0x76, 0x77, 0x79, 0x7a, 0x7b, 0x7c, 0x7d, 0x7e,
    0x7f, 0x81, 0x84, 0x85, 0x87, 0x89, 0x91, 0x94, 0x95, 0x97, 0x9c, 0x9f, 0xa4, 0xa5, 0xa6, 0xa7,
    0xac, 0xad, 0xae, 0xaf, 0xb4, 0xb5, 0xb6, 0xb7, 0xbc, 0xbd, 0xbe, 0xbf, 0xc3, 0xc4, 0xc5, 0xc6,
    0xc7, 0xcc, 0xcd, 0xce, 0xcf, 0xd4, 0xd5, 0xd6, 0xd7, 0xdb, 0xdc, 0xdd, 0xde,
];

#[cfg(test)]
mod tests {
    use super::*;

    fn slot(opc: u8, dst: u8, src: u8, off: i16, imm: i32) -> [u8; INSN_SIZE] {
        let mut bytes = [0u8; INSN_SIZE];
        bytes[0] = opc;
        bytes[1] = (src << 4) | (dst & 0x0f);
        bytes[2..4].copy_from_slice(&off.to_le_bytes());
        bytes[4..8].copy_from_slice(&imm.to_le_bytes());
        bytes
    }

    fn ld_dw_imm(src: u8, imm: i32, high: i32) -> [u8; 2 * INSN_SIZE] {
        let mut bytes = [0u8; 2 * INSN_SIZE];
        bytes[..INSN_SIZE].copy_from_slice(&slot(LD_DW_IMM_OPC, 1, src, 0, imm));
        bytes[INSN_SIZE + 4..].copy_from_slice(&high.to_le_bytes());
        bytes
    }

    #[test]
    fn truncated_ld_dw_imm_is_rejected() {
        let bytes = slot(LD_DW_IMM_OPC, 1, 0, 0, 0);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
    }

    #[test]
    fn first_slot_offset_and_second_slot_reserved_fields_are_rejected() {
        let mut bytes = ld_dw_imm(0, 0, 0);
        bytes[2] = 1;
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
        let mut bytes = ld_dw_imm(BPF_PSEUDO_MAP_FD as u8, 7, 0);
        bytes[3] = 0x80;
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
    }

    #[test]
    fn reserved_second_slot_fields_are_rejected() {
        let mut bytes = ld_dw_imm(0, 0, 0);
        bytes[INSN_SIZE] = 0x95;
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
        let mut bytes = ld_dw_imm(0, 0, 0);
        bytes[INSN_SIZE + 1] = 1;
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
        let mut bytes = ld_dw_imm(0, 0, 0);
        bytes[INSN_SIZE + 3] = 1;
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
    }

    #[test]
    fn plain_and_map_immediates_are_classified() {
        let bytes = ld_dw_imm(0, 0x1122_3344, 0x5566_7788);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Ok(LdDwImm::Imm64));

        let bytes = ld_dw_imm(BPF_PSEUDO_MAP_FD as u8, 7, 0);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Ok(LdDwImm::MapFd { fd: 7 }));

        let bytes = ld_dw_imm(BPF_PSEUDO_MAP_FD as u8, 7, 1);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));

        let bytes = ld_dw_imm(BPF_PSEUDO_MAP_VALUE as u8, 7, 3);
        assert_eq!(
            decode_ld_dw_imm(&bytes, 0),
            Ok(LdDwImm::MapValue { fd: 7, offset: 3 })
        );
    }

    #[test]
    fn unsupported_pseudo_classes_are_rejected() {
        let bytes = ld_dw_imm(BPF_PSEUDO_MAP_IDX as u8, 0, 0);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EPROTO));
        let bytes = ld_dw_imm(BPF_PSEUDO_MAP_IDX as u8, 0, 1);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
        let bytes = ld_dw_imm(BPF_PSEUDO_MAP_IDX_VALUE as u8, 0, 0);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EPROTO));
        let bytes = ld_dw_imm(7, 0, 0);
        assert_eq!(decode_ld_dw_imm(&bytes, 0), Err(SystemError::EINVAL));
    }

    #[test]
    fn write_imm64_round_trips_through_the_second_slot() {
        let mut bytes = ld_dw_imm(0, 0, 0);
        let value = 0xffff_8888_1234_5678u64;
        write_imm64(&mut bytes, 0, value);
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()),
            value as u32
        );
        assert_eq!(
            u32::from_le_bytes(bytes[INSN_SIZE + 4..].try_into().unwrap()),
            (value >> 32) as u32
        );
    }

    #[test]
    fn ld_dw_imm_is_the_only_two_slot_opcode() {
        let single = slot(rbpf::ebpf::MOV64_IMM, 0, 0, 0, 1);
        assert_eq!(check_slot(&single, 0), Ok(false));
        let double = ld_dw_imm(0, 1, 0);
        assert_eq!(check_slot(&double, 0), Ok(true));
    }

    #[test]
    fn reserved_ldx_fields_are_rejected() {
        // `LDX` with a non-`MEM` mode.
        let bad_mode = slot(BPF_LDX | rbpf::ebpf::BPF_ABS, 0, 0, 0, 0);
        assert_eq!(check_slot(&bad_mode, 0), Err(SystemError::EINVAL));
        // `LDX` with a non-zero immediate.
        let bad_imm = slot(BPF_LDX | BPF_MEM | rbpf::ebpf::BPF_W, 0, 0, 0, 1);
        assert_eq!(check_slot(&bad_imm, 0), Err(SystemError::EINVAL));
        let ok = slot(BPF_LDX | BPF_MEM | rbpf::ebpf::BPF_W, 0, 0, 0, 0);
        assert_eq!(check_slot(&ok, 0), Ok(false));
        let ok = slot(BPF_LDX | BPF_MEMSX | rbpf::ebpf::BPF_W, 0, 0, 0, 0);
        assert_eq!(check_slot(&ok, 0), Ok(false));
    }

    #[test]
    fn unknown_opcodes_are_rejected() {
        // `0x00` is the cBPF `LD | IMM | W` form; eBPF only defines the
        // `DW` width for `LD | IMM`.
        let unknown = slot(0x00, 0, 0, 0, 0);
        assert_eq!(check_slot(&unknown, 0), Err(SystemError::EINVAL));
        // `ST` without a memory-mode address is not an eBPF instruction.
        let unknown = slot(rbpf::ebpf::BPF_ST, 0, 0, 0, 0);
        assert_eq!(check_slot(&unknown, 0), Err(SystemError::EINVAL));

        assert!(is_known_opcode(rbpf::ebpf::EXIT));
        assert!(is_known_opcode(rbpf::ebpf::LD_DW_IMM));
        assert!(is_known_opcode(rbpf::ebpf::LD_ABS_W));
        assert!(is_known_opcode(rbpf::ebpf::MOV64_IMM));
        assert!(!is_known_opcode(0x00));
        assert!(!is_known_opcode(rbpf::ebpf::BPF_ST));
    }

    #[test]
    fn insn_table_is_sorted_and_unique() {
        assert!(INSN_TABLE.windows(2).all(|pair| pair[0] < pair[1]));
    }
}
