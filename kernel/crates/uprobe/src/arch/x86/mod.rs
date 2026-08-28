//! x86_64 instruction analysis and XOL slot copy generation.
//!
//! Reuses the yaxpeax-x86 decoder that kprobe already depends on:
//! - decode with `InstDecoder::default().decode_slice(bytes)` and take
//!   `.len().to_const()` for the instruction length (consistent with kprobe's
//!   `arch/x86/mod.rs` usage);
//! - iterate over the operands to detect RIP-relative addressing (both
//!   `[rip+disp]` and `[rip]` forms are handled);
//! - XOL slot copy generation happens in two steps: static analysis produces
//!   [`InsnAnalysis`], and at runtime the mm layer calls [`build_xol_slot`]
//!   with the real slot address to perform the RIP-relative relocation.

use ::core::convert::TryFrom;

use yaxpeax_arch::{DecodeError, LengthedInstruction};
use yaxpeax_x86::amd64::{Instruction, Operand, RegSpec};

/// Maximum length of a single x86_64 instruction, including prefixes.
const MAX_INSN_SIZE: usize = 15;

/// uprobe instruction analysis error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UprobeInsnError {
    /// The input bytes are not enough to decode a complete instruction.
    Truncated,
    /// yaxpeax decoding failed (invalid opcode / operand / prefix, etc.).
    DecodeFailed,
    /// Decoded length exceeds the x86_64 limit (15 bytes).
    TooLong,
    /// The displacement after RIP-relative relocation overflows the i32 range
    /// (does not fit in a disp32).
    DisplacementOverflow,
    /// Address-size override selected EIP-relative addressing. Phase one does
    /// not implement the required 32-bit wrapping relocation semantics.
    UnsupportedEipRelative,
    /// Control-flow instructions (call/jmp/ret/jcc/loop/int, etc.) — execution
    /// leaves the XOL slot, so a subsequent #DB cannot infer the probe address,
    /// and the stack/control flow may be corrupted. Rejected at registration.
    UnsupportedControlFlow,
    /// Instructions that suppress #DB (MOV SS/POP SS), observe the transient TF
    /// (PUSHF*), or rewrite RFLAGS wholesale (POPF*) — XOL single-stepping
    /// would alter user-visible state or lose the #DB. Rejected at registration.
    UnsafeForXol,
    /// REP/REPE/REPNE string instructions may report an intermediate #DB
    /// with RIP still at the copied instruction. The phase-1 exact-end XOL
    /// state machine cannot complete those iterations safely.
    UnsupportedRepeatedString,
}

/// RIP-relative relocation information (derived statically; applied at runtime
/// with the real slot address).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RipReloc {
    /// Byte offset of the 4-byte signed displacement within the instruction.
    pub disp_offset: usize,
    /// The original signed displacement obtained by decoding.
    pub disp: i32,
}

/// Static analysis result of an instruction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsnAnalysis {
    /// Decoded length (1..=15).
    pub insn_len: usize,
    /// If this is a RIP-relative instruction, the relocation information;
    /// otherwise `None`.
    pub rip_relative: Option<RipReloc>,
}

/// Return the exact inclusive range of XOL slot addresses from which this
/// instruction can be relocated without overflowing its encoded `disp32`.
///
/// For a RIP-relative instruction, [`build_xol_slot`] computes
/// `new_disp = old_disp + probe_vaddr - slot_vaddr`. Solving that expression
/// for every value representable by `i32` gives this interval. Non-RIP-relative
/// instructions have no placement constraint.
pub fn xol_slot_vaddr_range(
    analysis: &InsnAnalysis,
    probe_vaddr: usize,
) -> core::ops::RangeInclusive<usize> {
    let Some(reloc) = analysis.rip_relative else {
        return 0..=usize::MAX;
    };

    // i128 keeps both signed disp32 endpoints and the full usize address
    // domain representable without wrapping at either user-address boundary.
    let center = probe_vaddr as i128 + reloc.disp as i128;
    let first = center - i32::MAX as i128;
    let last = center - i32::MIN as i128;
    let first = first.clamp(0, usize::MAX as i128) as usize;
    let last = last.clamp(0, usize::MAX as i128) as usize;
    first..=last
}

/// Decode and statically analyze a single x86_64 instruction.
///
/// `bytes` must contain at least the full instruction (extra bytes are
/// ignored). Returns the instruction length and, if present, the
/// RIP-relative relocation information.
///
/// # Fail-fast
/// Truncated bytes / decode failure / length overflow → the corresponding
/// error is returned, and the caller abandons that probe point.
pub fn analyze_insn(bytes: &[u8]) -> Result<InsnAnalysis, UprobeInsnError> {
    if bytes.is_empty() {
        return Err(UprobeInsnError::Truncated);
    }
    let decoder = yaxpeax_x86::amd64::InstDecoder::default();
    let inst = decoder.decode_slice(bytes).map_err(|e| {
        if e.data_exhausted() {
            UprobeInsnError::Truncated
        } else {
            UprobeInsnError::DecodeFailed
        }
    })?;
    let insn_len = inst.len().to_const() as usize;
    if insn_len == 0 || insn_len > MAX_INSN_SIZE {
        return Err(UprobeInsnError::TooLong);
    }
    if bytes.len() < insn_len {
        return Err(UprobeInsnError::Truncated);
    }
    // Control-flow / unsafe instructions would break the single-step window
    // when executed from the XOL slot, so they are rejected at registration:
    // - Control flow (jump/call/return/loop/interrupt/syscall): execution
    //   leaves the slot, so the #DB cannot be captured inside it, and call/ret
    //   would also corrupt the user stack (consistent with the Linux uprobe
    //   phases: boost/add_on_return are not in scope).
    // - MOV SS / POP SS: the Intel SDM specifies that #DB is suppressed up to
    //   the following instruction boundary — the #DB completing the XOL
    //   single-step would be lost (review R10).
    // - PUSHF: pushes the TF temporarily set by uprobe onto the user stack,
    //   changing user-visible results.
    // - POPF: rewrites RFLAGS wholesale, clearing the TF set by uprobe and
    //   breaking the single-step window (review R10).
    if is_control_flow(&inst) {
        return Err(UprobeInsnError::UnsupportedControlFlow);
    }
    if suppresses_debug_or_rewrites_flags(&inst) {
        return Err(UprobeInsnError::UnsafeForXol);
    }
    if is_repeated_string(&inst) {
        return Err(UprobeInsnError::UnsupportedRepeatedString);
    }

    let rip_relative = find_rip_relative(&inst, insn_len)?;
    Ok(InsnAnalysis {
        insn_len,
        rip_relative,
    })
}

fn is_repeated_string(inst: &Instruction) -> bool {
    use yaxpeax_x86::amd64::Opcode;

    inst.prefixes.rep_any()
        && matches!(
            inst.opcode(),
            Opcode::CMPS
                | Opcode::SCAS
                | Opcode::MOVS
                | Opcode::LODS
                | Opcode::STOS
                | Opcode::INS
                | Opcode::OUTS
        )
}

/// Determine whether an instruction is a control-flow instruction (not safely
/// executable from an XOL slot).
///
/// Covers: direct jumps/calls/returns, conditional jumps (Jcc), loops (LOOP*),
/// transactional branches (XBEGIN), interrupts (INT/INT3/IRET*), and
/// syscall/sysret (SYSCALL/SYSRET/SYSENTER/SYSEXIT).
/// These instructions change RIP in a way that prevents the #DB after XOL
/// single-stepping from being captured inside the slot, or they write the XOL
/// address to the user stack and corrupt control flow.
fn is_control_flow(inst: &Instruction) -> bool {
    use yaxpeax_x86::amd64::Opcode;
    matches!(
        inst.opcode(),
        Opcode::CALL
            | Opcode::CALLF
            | Opcode::JMP
            | Opcode::JMPF
            | Opcode::RETURN
            | Opcode::RETF
            | Opcode::LOOPNZ
            | Opcode::LOOPZ
            | Opcode::LOOP
            | Opcode::JRCXZ
            | Opcode::XBEGIN
            | Opcode::INT
            | Opcode::IRET
            | Opcode::IRETD
            | Opcode::IRETQ
            | Opcode::SYSCALL
            | Opcode::SYSRET
            | Opcode::SYSENTER
            | Opcode::SYSEXIT
    ) || inst.opcode().is_jcc()
}

fn suppresses_debug_or_rewrites_flags(inst: &Instruction) -> bool {
    use yaxpeax_x86::amd64::{Opcode, RegSpec};
    if matches!(inst.opcode(), Opcode::PUSHF | Opcode::POPF) {
        return true;
    }
    // `MOV SS, r/m16` (8e /r): after loading SS, #DB is suppressed up to the
    // next instruction boundary. yaxpeax decodes it as `Opcode::MOV` with the
    // destination operand being the ss segment register.
    // (`POP SS`, 0x17, is an invalid encoding in 64-bit mode; the decoder
    // errors out directly, so no handling is needed.)
    if inst.opcode() == Opcode::MOV {
        if let Operand::Register { reg } = inst.operand(0) {
            return reg == RegSpec::ss();
        }
    }
    false
}

/// Find a RIP-relative memory operand in a decoded instruction and return the
/// relocation information.
///
/// Must cover **every** operand form whose base is RIP: yaxpeax yields
/// `Disp { base: RIP, disp }` for `[rip+disp]` and `MemDeref { base: RIP }`
/// for `[rip]` (disp == 0). Missing either form would make the XOL slot
/// execute with the original disp, pointing at the wrong address (silent
/// corruption), so any RIP form that cannot be relocated safely (masked /
/// with index) fails fast.
fn find_rip_relative(
    inst: &Instruction,
    insn_len: usize,
) -> Result<Option<RipReloc>, UprobeInsnError> {
    for i in 0..inst.operand_count() {
        if let Some(disp) = operand_rip_disp(&inst.operand(i))? {
            // [rip+disp32] encoding: the displacement is always 4 bytes and
            // precedes any trailing immediate. Hence
            // disp_offset = insn_len - 4 - imm_size.
            let imm_size = trailing_immediate_size(inst);
            if imm_size + 4 > insn_len {
                // Structurally abnormal (should not happen in theory); fail
                // conservatively.
                return Err(UprobeInsnError::DecodeFailed);
            }
            let disp_offset = insn_len - 4 - imm_size;
            return Ok(Some(RipReloc { disp_offset, disp }));
        }
    }
    Ok(None)
}

/// Determine whether a single operand is RIP-relative:
/// - `Ok(Some(disp))`: yes, with the signed displacement (`[rip]` counts as disp=0);
/// - `Ok(None)`: no;
/// - `Err`: RIP-relative but an unusual masked / indexed form that cannot be
///   relocated safely.
fn operand_rip_disp(op: &Operand) -> Result<Option<i32>, UprobeInsnError> {
    match op {
        Operand::MemDeref { base } if *base == RegSpec::RIP => Ok(Some(0)),
        Operand::Disp { base, disp } if *base == RegSpec::RIP => Ok(Some(*disp)),
        Operand::MemDeref { base } | Operand::Disp { base, .. } if *base == RegSpec::eip() => {
            Err(UprobeInsnError::UnsupportedEipRelative)
        }
        // Standard RIP-relative addressing has no SIB index and no mask;
        // matching these forms fails fast.
        Operand::DispMasked { base, .. }
        | Operand::MemDerefMasked { base, .. }
        | Operand::MemBaseIndexScale { base, .. }
        | Operand::MemBaseIndexScaleDisp { base, .. }
        | Operand::MemBaseIndexScaleMasked { base, .. }
        | Operand::MemBaseIndexScaleDispMasked { base, .. }
            if *base == RegSpec::RIP =>
        {
            Err(UprobeInsnError::DecodeFailed)
        }
        Operand::DispMasked { base, .. }
        | Operand::MemDerefMasked { base, .. }
        | Operand::MemBaseIndexScale { base, .. }
        | Operand::MemBaseIndexScaleDisp { base, .. }
        | Operand::MemBaseIndexScaleMasked { base, .. }
        | Operand::MemBaseIndexScaleDispMasked { base, .. }
            if *base == RegSpec::eip() =>
        {
            Err(UprobeInsnError::UnsupportedEipRelative)
        }
        _ => Ok(None),
    }
}

/// Compute the byte size of the instruction's trailing immediate (used to
/// locate the displacement offset of a [rip+disp32]).
///
/// The fixed x86 encoding order is: prefixes / opcode / ModRM / [SIB] /
/// [disp] / [imm], so disp immediately precedes imm. An instruction with a
/// [rip+disp32] memory operand has at most one immediate.
fn trailing_immediate_size(inst: &Instruction) -> usize {
    for i in 0..inst.operand_count() {
        match inst.operand(i) {
            Operand::ImmediateI8 { .. } | Operand::ImmediateU8 { .. } => return 1,
            Operand::ImmediateI16 { .. } | Operand::ImmediateU16 { .. } => return 2,
            Operand::ImmediateI32 { .. } | Operand::ImmediateU32 { .. } => return 4,
            // In a RIP-relative ModRM instruction, an I64 immediate is an
            // encoded imm32 that the decoder has widened to its signed
            // execution value (for example REX.W IMUL/MOV r/m64, imm32).
            // x86-64 has no RIP-relative memory form with an encoded imm64.
            Operand::ImmediateI64 { .. } => return 4,
            Operand::ImmediateU64 { .. } => return 8,
            _ => {}
        }
    }
    0
}

/// Generate the XOL slot copy (copy the original instruction and perform the
/// RIP-relative relocation).
///
/// # Parameters
/// - `analysis`: the result of [`analyze_insn`].
/// - `probe_vaddr`: the user virtual address of the original probe point.
/// - `slot_vaddr`: the real user virtual address of the XOL slot (per-mm,
///   supplied by the mm layer at runtime).
/// - `old_instruction`: the original instruction bytes (the first
///   `analysis.insn_len` bytes are valid).
/// - `slot`: the output buffer; its length must be >= `analysis.insn_len`.
///
/// # RIP-relative relocation
/// When the original instruction executes at `probe_vaddr`, the effective
/// address of `[rip+disp]` is `probe_vaddr + insn_len + disp` (rip points at
/// the next instruction). For the copy executing at `slot_vaddr` to keep the
/// same effective address, we need
/// `slot_vaddr + insn_len + new_disp = probe_vaddr + insn_len + disp`, i.e.
/// `new_disp = disp + (probe_vaddr - slot_vaddr)`. If `new_disp` overflows
/// the i32 range, the relocation fails.
pub fn build_xol_slot(
    analysis: &InsnAnalysis,
    probe_vaddr: usize,
    slot_vaddr: usize,
    old_instruction: &[u8],
    slot: &mut [u8],
) -> Result<(), UprobeInsnError> {
    let len = analysis.insn_len;
    if old_instruction.len() < len || slot.len() < len {
        return Err(UprobeInsnError::Truncated);
    }
    // Copy the original instruction.
    slot[..len].copy_from_slice(&old_instruction[..len]);

    // RIP-relative relocation.
    if let Some(reloc) = analysis.rip_relative {
        let delta = probe_vaddr as i64 - slot_vaddr as i64;
        let new_disp = reloc.disp as i64 + delta;
        let new_disp =
            i32::try_from(new_disp).map_err(|_| UprobeInsnError::DisplacementOverflow)?;
        slot[reloc.disp_offset..reloc.disp_offset + 4].copy_from_slice(&new_disp.to_le_bytes());
    }

    // Fill the trailing bytes after the original instruction with int3 (0xcc).
    //
    // Normal path: TF fires #DB immediately after the original instruction
    // executes, so the trailing bytes are never reached.
    // Race path: if this uprobe is unregistered after #BP but before #DB (the
    // slot is freed) and the slot is reassigned to another probe, the #DB
    // handler cannot infer probe_vaddr. If the thread keeps executing from
    // the slot, it will hit the trailing int3 → #BP fires again → normal
    // uprobe dispatch or SIGTRAP, instead of executing zero-fill bytes (which
    // could decode as unexpected instructions like `add [rax], al` and
    // corrupt memory).
    for b in &mut slot[len..] {
        *b = 0xcc;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insn_len_basic() {
        // nop
        assert_eq!(analyze_insn(&[0x90]).unwrap().insn_len, 1);
        // push rbp
        assert_eq!(analyze_insn(&[0x55]).unwrap().insn_len, 1);
        // mov rbp, rsp  (48 89 e5)
        assert_eq!(analyze_insn(&[0x48, 0x89, 0xe5]).unwrap().insn_len, 3);
    }

    #[test]
    fn no_rip_relative() {
        let a = analyze_insn(&[0x48, 0x89, 0xe5]).unwrap();
        assert_eq!(a.insn_len, 3);
        assert!(a.rip_relative.is_none());
        assert_eq!(xol_slot_vaddr_range(&a, usize::MAX), 0..=usize::MAX);
    }

    #[test]
    fn xol_slot_range_matches_disp32_endpoints() {
        let analysis = InsnAnalysis {
            insn_len: 7,
            rip_relative: Some(RipReloc {
                disp_offset: 3,
                disp: 0,
            }),
        };
        let probe = 0x1_0000_0000usize;
        let range = xol_slot_vaddr_range(&analysis, probe);
        assert_eq!(*range.start(), probe - i32::MAX as usize);
        assert_eq!(*range.end(), probe + (i32::MAX as usize + 1));

        let insn = [0x48, 0x8b, 0x05, 0, 0, 0, 0];
        let mut slot = [0u8; crate::UPROBE_INSN_COPY_SIZE];
        assert!(build_xol_slot(&analysis, probe, *range.start(), &insn, &mut slot).is_ok());
        assert!(build_xol_slot(&analysis, probe, *range.end(), &insn, &mut slot).is_ok());
        assert_eq!(
            build_xol_slot(&analysis, probe, range.start() - 1, &insn, &mut slot,),
            Err(UprobeInsnError::DisplacementOverflow)
        );
        assert_eq!(
            build_xol_slot(&analysis, probe, range.end() + 1, &insn, &mut slot),
            Err(UprobeInsnError::DisplacementOverflow)
        );
    }

    #[test]
    fn xol_slot_range_clips_without_wrapping() {
        let low = InsnAnalysis {
            insn_len: 7,
            rip_relative: Some(RipReloc {
                disp_offset: 3,
                disp: i32::MIN,
            }),
        };
        assert_eq!(*xol_slot_vaddr_range(&low, 0).start(), 0);

        let high = InsnAnalysis {
            insn_len: 7,
            rip_relative: Some(RipReloc {
                disp_offset: 3,
                disp: i32::MAX,
            }),
        };
        assert_eq!(*xol_slot_vaddr_range(&high, usize::MAX).end(), usize::MAX);
    }

    #[test]
    fn rip_relative_lea() {
        // lea rax, [rip+0x1234]  ->  48 8d 05 34 12 00 00
        let a = analyze_insn(&[0x48, 0x8d, 0x05, 0x34, 0x12, 0x00, 0x00]).unwrap();
        assert_eq!(a.insn_len, 7);
        let r = a.rip_relative.expect("lea rip-rel must be detected");
        // disp occupies the last 4 bytes, no trailing immediate.
        assert_eq!(r.disp_offset, 3);
        assert_eq!(r.disp, 0x1234);
    }

    #[test]
    fn eip_relative_addressing_is_rejected() {
        for bytes in [
            &[0x67, 0x8b, 0x05, 0x00, 0x00, 0x00, 0x00][..],
            &[0x67, 0x8b, 0x05, 0x34, 0x12, 0x00, 0x00][..],
            &[
                0x67, 0xc7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
            ][..],
        ] {
            assert_eq!(
                analyze_insn(bytes).unwrap_err(),
                UprobeInsnError::UnsupportedEipRelative,
                "bytes={bytes:x?}"
            );
        }

        // Address-size override alone is safe when the address is based on a
        // general-purpose register rather than the slot-relative next EIP.
        assert!(analyze_insn(&[0x67, 0x8b, 0x00]).is_ok());
    }

    #[test]
    fn rip_relative_with_immediate() {
        // mov dword [rip+0x10], 5  ->  c7 05 10 00 00 00 05 00 00 00
        // disp32 precedes the imm32; disp_offset = len(10) - 4 - imm(4) = 2.
        let bytes = [0xc7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00];
        let a = analyze_insn(&bytes).unwrap();
        assert_eq!(a.insn_len, 10);
        let r = a.rip_relative.expect("mov [rip+disp],imm must be detected");
        assert_eq!(r.disp_offset, 2);
        assert_eq!(r.disp, 0x10);
    }

    #[test]
    fn rip_relative_sign_extended_imm32_uses_encoded_width() {
        // In 64-bit operand mode yaxpeax exposes these sign-extended imm32
        // values as ImmediateI64. Their encoded width is still four bytes.
        for (bytes, expected_offset) in [
            (
                &[
                    0x48, 0x69, 0x05, 0x10, 0x00, 0x00, 0x00, 0xfb, 0xff, 0xff, 0xff,
                ][..],
                3,
            ),
            (
                &[
                    0x2e, 0x48, 0x69, 0x05, 0x10, 0x00, 0x00, 0x00, 0xfb, 0xff, 0xff, 0xff,
                ][..],
                4,
            ),
            (
                &[
                    0x48, 0xc7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
                ][..],
                3,
            ),
            (
                &[
                    0x2e, 0x48, 0xc7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
                ][..],
                4,
            ),
        ] {
            let analysis = analyze_insn(bytes).unwrap();
            let reloc = analysis.rip_relative.expect("RIP-relative operand");
            assert_eq!(reloc.disp_offset, expected_offset, "bytes={bytes:x?}");
            assert_eq!(reloc.disp, 0x10, "bytes={bytes:x?}");
        }
    }

    #[test]
    fn xol_relocation_preserves_prefixed_opcode_and_immediate() {
        for bytes in [
            &[
                0x2e, 0x48, 0x69, 0x05, 0x10, 0x00, 0x00, 0x00, 0xfb, 0xff, 0xff, 0xff,
            ][..],
            &[
                0x2e, 0x48, 0xc7, 0x05, 0x10, 0x00, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00,
            ][..],
        ] {
            let analysis = analyze_insn(bytes).unwrap();
            let reloc = analysis.rip_relative.expect("RIP-relative operand");
            let mut slot = [0xcc; MAX_INSN_SIZE];
            build_xol_slot(&analysis, 0x2000, 0x1000, bytes, &mut slot).unwrap();

            assert_eq!(&slot[..reloc.disp_offset], &bytes[..reloc.disp_offset]);
            assert_eq!(
                &slot[reloc.disp_offset + 4..analysis.insn_len],
                &bytes[reloc.disp_offset + 4..analysis.insn_len]
            );
            assert_eq!(
                &slot[reloc.disp_offset..reloc.disp_offset + 4],
                &0x1010_i32.to_le_bytes()
            );
        }
    }

    #[test]
    fn control_flow_rejected() {
        // call rel32 (e8), jmp rel8 (eb), ret (c3), je rel8 (74),
        // loopnz/loopz/loop/jrcxz (e0..e3), xbegin rel32 (c7 f8),
        // ICEBP/INT1 (f1), syscall/sysret (0f 05/07),
        // sysenter/sysexit (0f 34/35)
        for bytes in [
            &[0xe8, 0x00, 0x00, 0x00, 0x00][..],
            &[0xeb, 0xfe][..],
            &[0xc3][..],
            &[0x74, 0x02][..],
            &[0xe0, 0x00][..],
            &[0xe1, 0x00][..],
            &[0xe2, 0x00][..],
            &[0xe3, 0x00][..],
            &[0x67, 0xe0, 0x00][..],
            &[0x67, 0xe1, 0x00][..],
            &[0x67, 0xe2, 0x00][..],
            &[0x67, 0xe3, 0x00][..],
            &[0xc7, 0xf8, 0x00, 0x00, 0x00, 0x00][..],
            &[0xf1][..],
            &[0x0f, 0x05][..],
            &[0x0f, 0x07][..],
            &[0x0f, 0x34][..],
            &[0x0f, 0x35][..],
        ] {
            assert_eq!(
                analyze_insn(bytes).unwrap_err(),
                UprobeInsnError::UnsupportedControlFlow,
                "bytes={bytes:x?}"
            );
        }
    }

    #[test]
    fn debug_suppressing_rejected() {
        // pushfq (9c); popfq (9d); mov ss, rax (8e d0)
        assert_eq!(
            analyze_insn(&[0x9c]).unwrap_err(),
            UprobeInsnError::UnsafeForXol
        );
        assert_eq!(
            analyze_insn(&[0x9d]).unwrap_err(),
            UprobeInsnError::UnsafeForXol
        );
        assert_eq!(
            analyze_insn(&[0x8e, 0xd0]).unwrap_err(),
            UprobeInsnError::UnsafeForXol
        );
        // Control: mov ds, eax (8e d8, segment encoding 3 = DS, not SS) does
        // not suppress #DB, so it is accepted
        assert!(analyze_insn(&[0x8e, 0xd8]).is_ok());
        // Control: an ordinary mov (non-SS destination) is accepted
        assert!(analyze_insn(&[0x48, 0x89, 0xe5]).is_ok());
        // Intel explicitly specifies that LSS does not have the event-suppression
        // behavior of MOV SS/POP SS; Linux's insn_masking_exception() does not
        // reject it either, so we must not widen the rejection here.
        assert!(analyze_insn(&[0x0f, 0xb2, 0x00]).is_ok());
    }

    #[test]
    fn repeated_string_instructions_are_rejected() {
        for bytes in [
            &[0xf3, 0xa4][..], // rep movsb
            &[0xf2, 0xa6][..], // repne cmpsb
            &[0xf3, 0xae][..], // repe scasb
        ] {
            assert_eq!(
                analyze_insn(bytes).unwrap_err(),
                UprobeInsnError::UnsupportedRepeatedString,
                "bytes={bytes:x?}"
            );
        }

        // F3 is also a mandatory/semantic prefix for non-string instructions.
        // Do not reject PAUSE merely because it shares the REP byte.
        assert!(analyze_insn(&[0xf3, 0x90]).is_ok());
    }
    #[test]
    fn build_slot_relocates_disp() {
        // lea rax, [rip+0]  ->  48 8d 05 00 00 00 00
        let insn: [u8; 7] = [0x48, 0x8d, 0x05, 0x00, 0x00, 0x00, 0x00];
        let a = analyze_insn(&insn).unwrap();
        let probe_vaddr: usize = 0x1000;
        let slot_vaddr: usize = 0x2000;
        let mut slot = [0u8; 16];
        build_xol_slot(&a, probe_vaddr, slot_vaddr, &insn, &mut slot).unwrap();

        // new_disp = 0 + (0x1000 - 0x2000) = -0x1000; opcode/ModRM unchanged,
        // only disp is rewritten.
        assert_eq!(&slot[..3], &insn[..3]);
        assert_eq!(&slot[3..7], &(-0x1000i32).to_le_bytes());

        // Executing from the slot still points at the original effective
        // address: slot+7+new_disp == probe+7+0
        let new_disp = i32::from_le_bytes([slot[3], slot[4], slot[5], slot[6]]);
        let eff = slot_vaddr as i64 + 7 + new_disp as i64;
        assert_eq!(eff, probe_vaddr as i64 + 7);
    }

    #[test]
    fn build_slot_displacement_overflow() {
        let insn: [u8; 7] = [0x48, 0x8d, 0x05, 0x00, 0x00, 0x00, 0x00];
        let a = analyze_insn(&insn).unwrap();
        let mut slot = [0u8; 16];
        // The span exceeds the i32 range -> relocation fails.
        let huge = (i32::MAX as usize) + 2;
        let err = build_xol_slot(&a, huge, 0, &insn, &mut slot).unwrap_err();
        assert_eq!(err, UprobeInsnError::DisplacementOverflow);
    }

    #[test]
    fn analyze_errors() {
        // Empty input.
        assert_eq!(analyze_insn(&[]).unwrap_err(), UprobeInsnError::Truncated);
        // Incomplete instruction (call rel32 has only the opcode, missing the
        // 4-byte imm) -> the decoder exhausts the input.
        assert_eq!(
            analyze_insn(&[0xe8]).unwrap_err(),
            UprobeInsnError::Truncated
        );
        // Invalid opcode (push es is invalid in 64-bit long mode).
        assert_eq!(
            analyze_insn(&[0x06]).unwrap_err(),
            UprobeInsnError::DecodeFailed
        );
    }
}
