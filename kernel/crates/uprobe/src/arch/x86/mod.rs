//! x86_64 指令分析与 XOL slot 副本生成。
//!
//! 直接复用 yaxpeax-x86（kprobe 已依赖）解码器：
//! - 用 `InstDecoder::default().decode_slice(bytes)` 解码，取 `.len().to_const()` 得
//!   指令长度（与 kprobe `arch/x86/mod.rs` 用法一致）；
//! - 遍历操作数检测 RIP-relative（`[rip+disp]` 与 `[rip]` 两种呈现均处理）；
//! - XOL slot 副本生成分两步：静态分析产出 [`InsnAnalysis`]，运行时用真实 slot 地址
//!   调用 [`build_xol_slot`] 做 RIP-relative 重定位（由 mm 层在命中时调用）。

use ::core::convert::TryFrom;

use yaxpeax_arch::{DecodeError, LengthedInstruction};
use yaxpeax_x86::amd64::{Instruction, Operand, RegSpec};

/// x86_64 单条指令最大长度（含前缀）。
const MAX_INSN_SIZE: usize = 15;

/// uprobe 指令分析错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UprobeInsnError {
    /// 输入字节数不足以解码一条完整指令。
    Truncated,
    /// yaxpeax 解码失败（非法操作码 / 操作数 / 前缀等）。
    DecodeFailed,
    /// 解码长度超过 x86_64 上限（15 字节）。
    TooLong,
    /// RIP-relative 重定位后的位移超出 i32 范围（disp32 装不下）。
    DisplacementOverflow,
    /// Address-size override selected EIP-relative addressing. Phase one does
    /// not implement the required 32-bit wrapping relocation semantics.
    UnsupportedEipRelative,
    /// 控制流指令（call/jmp/ret/jcc/loop/int 等）——XOL 执行会跳出 slot，
    /// 后续 #DB 无法反推探针址，且可能损坏栈/控制流。注册时拒绝。
    UnsupportedControlFlow,
    /// 指令抑制 #DB（MOV SS/POP SS）、观察临时 TF（PUSHF*）或整体改写
    /// RFLAGS（POPF*）——XOL 单步会改变用户可见状态或丢失 #DB。注册时拒绝。
    UnsafeForXol,
    /// REP/REPE/REPNE string instructions may report an intermediate #DB
    /// with RIP still at the copied instruction. The phase-1 exact-end XOL
    /// state machine cannot complete those iterations safely.
    UnsupportedRepeatedString,
}

/// RIP-relative 重定位信息（静态分析得出，运行时用真实 slot 地址套用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RipReloc {
    /// 4 字节有符号位移在指令内的字节偏移。
    pub disp_offset: usize,
    /// 解码得到的原始有符号位移。
    pub disp: i32,
}

/// 指令静态分析结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsnAnalysis {
    /// 解码长度（1..=15）。
    pub insn_len: usize,
    /// 若为 RIP-relative 指令，给出重定位所需信息；否则为 `None`。
    pub rip_relative: Option<RipReloc>,
}

/// 解码并静态分析一条 x86_64 指令。
///
/// `bytes` 至少应包含完整指令（多余字节被忽略）。返回指令长度与（若存在的）
/// RIP-relative 重定位信息。
///
/// # Fail-fast
/// 字节不足 / 解码失败 / 长度超限 → 返回对应错误，调用方据此放弃该探测点。
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
    // 控制流/不安全指令从 XOL slot 执行会破坏单步窗口，注册时拒绝：
    // - 控制流（跳转/调用/返回/循环/中断/系统调用）：跳出 slot，#DB 无法
    //   在 slot 内捕获，call/ret 还会损坏用户栈（与 Linux uprobe 阶段一致：
    //   boost/add_on_return 不在本范围）。
    // - MOV SS / POP SS：Intel SDM 规定其后的指令边界抑制 #DB——XOL 单步
    //   完成的 #DB 会丢失（评审 R10）。
    // - PUSHF：会把 uprobe 临时设置的 TF 压入用户栈，改变用户可见结果。
    // - POPF：整体覆写 RFLAGS，清掉 uprobe 置的 TF，单步窗口断裂（评审 R10）。
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

/// 判断指令是否为控制流指令（不可从 XOL slot 安全执行）。
///
/// 覆盖：直接跳转/调用/返回、条件跳转（Jcc）、循环（LOOP*）、
/// 事务分支（XBEGIN）、中断（INT/INT3/IRET*）、系统调用/返回（SYSCALL/SYSRET）。
/// 这些指令改变 RIP 的方式使 XOL 单步后的 #DB 无法在 slot 内捕获，
/// 或会向用户栈写入 XOL 地址损坏控制流。
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
    ) || inst.opcode().is_jcc()
}

fn suppresses_debug_or_rewrites_flags(inst: &Instruction) -> bool {
    use yaxpeax_x86::amd64::{Opcode, RegSpec};
    if matches!(inst.opcode(), Opcode::PUSHF | Opcode::POPF) {
        return true;
    }
    // `MOV SS, r/m16`（8e /r）：装载 SS 后到下一条指令边界之间 #DB 被抑制。
    // yaxpeax 将其解码为 `Opcode::MOV` + 目标操作数为 ss 段寄存器。
    // （`POP SS` 0x17 在 64 位模式为非法编码，解码器直接报错，无需处理。）
    if inst.opcode() == Opcode::MOV {
        if let Operand::Register { reg } = inst.operand(0) {
            return reg == RegSpec::ss();
        }
    }
    false
}

/// 在已解码指令中查找 RIP-relative 内存操作数，返回重定位信息。
///
/// 必须覆盖**所有** base 为 RIP 的操作数呈现：yaxpeax 对 `[rip+disp]` 给出
/// `Disp { base: RIP, disp }`，对 `[rip]`（disp 为 0）给出 `MemDeref { base: RIP }`。
/// 漏判任一形式都会导致 XOL slot 用原始 disp 执行、指向错误地址（静默损坏），故对
/// 无法安全重定位的 RIP 形式（掩码 / 带 index）一律 fail-fast。
fn find_rip_relative(
    inst: &Instruction,
    insn_len: usize,
) -> Result<Option<RipReloc>, UprobeInsnError> {
    for i in 0..inst.operand_count() {
        if let Some(disp) = operand_rip_disp(&inst.operand(i))? {
            // [rip+disp32] 编码：位移恒为 4 字节，且位于任何尾随立即数之前。
            // 故 disp_offset = insn_len - 4 - imm_size。
            let imm_size = trailing_immediate_size(inst);
            if imm_size + 4 > insn_len {
                // 结构异常（理论不应发生），保守失败。
                return Err(UprobeInsnError::DecodeFailed);
            }
            let disp_offset = insn_len - 4 - imm_size;
            return Ok(Some(RipReloc { disp_offset, disp }));
        }
    }
    Ok(None)
}

/// 判定单个操作数是否为 RIP-relative：
/// - `Ok(Some(disp))`：是，给出有符号位移（`[rip]` 视为 disp=0）；
/// - `Ok(None)`：否；
/// - `Err`：是 RIP-relative 但属掩码 / 带 index 的非常规形式，无法安全重定位。
fn operand_rip_disp(op: &Operand) -> Result<Option<i32>, UprobeInsnError> {
    match op {
        Operand::MemDeref { base } if *base == RegSpec::RIP => Ok(Some(0)),
        Operand::Disp { base, disp } if *base == RegSpec::RIP => Ok(Some(*disp)),
        Operand::MemDeref { base } | Operand::Disp { base, .. } if *base == RegSpec::eip() => {
            Err(UprobeInsnError::UnsupportedEipRelative)
        }
        // 标准 RIP-relative 不带 SIB index、不带掩码；命中这些形式即 fail-fast。
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

/// 计算指令尾随立即数的字节数（用于定位 [rip+disp32] 的位移偏移）。
///
/// x86 编码顺序固定为：前缀 / 操作码 / ModRM / [SIB] / [disp] / [imm]，
/// 故 disp 紧邻 imm 之前。对含 [rip+disp32] 内存操作数的指令，至多一个立即数。
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

/// 生成 XOL slot 副本（复制原指令并对 RIP-relative 做重定位）。
///
/// # 参数
/// - `analysis`：[`analyze_insn`] 的结果。
/// - `probe_vaddr`：原探测点用户虚拟地址。
/// - `slot_vaddr`：XOL slot 的真实用户虚拟地址（per-mm，运行时由 mm 层给出）。
/// - `old_instruction`：原指令字节（前 `analysis.insn_len` 字节有效）。
/// - `slot`：输出缓冲，长度须 >= `analysis.insn_len`。
///
/// # RIP-relative 重定位
/// 原指令在 `probe_vaddr` 执行时，`[rip+disp]` 的有效地址为
/// `probe_vaddr + insn_len + disp`（rip 指向下一条指令）。副本在 `slot_vaddr`
/// 执行时，欲保持同一有效地址，需满足
/// `slot_vaddr + insn_len + new_disp = probe_vaddr + insn_len + disp`，即
/// `new_disp = disp + (probe_vaddr - slot_vaddr)`。若 `new_disp` 超出 i32 范围则失败。
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
    // 复制原指令。
    slot[..len].copy_from_slice(&old_instruction[..len]);

    // RIP-relative 重定位。
    if let Some(reloc) = analysis.rip_relative {
        let delta = probe_vaddr as i64 - slot_vaddr as i64;
        let new_disp = reloc.disp as i64 + delta;
        let new_disp =
            i32::try_from(new_disp).map_err(|_| UprobeInsnError::DisplacementOverflow)?;
        slot[reloc.disp_offset..reloc.disp_offset + 4].copy_from_slice(&new_disp.to_le_bytes());
    }

    // 原指令之后的尾随字节填 int3(0xcc)。
    //
    // 正常路径：TF 在原指令执行后立即触发 #DB，不会执行到尾随字节。
    // 竞态路径：若 #BP 后、#DB 前该 uprobe 被注销（slot 被释放），且 slot
    // 被重分配给另一探针，#DB handler 无法反推 probe_vaddr。此时线程从 slot
    // 继续执行会命中尾随 int3 → 再次触发 #BP → 正常 uprobe 分发或 SIGTRAP，
    // 而非执行零填充（可能解码为 add [rax], al 等意外指令损坏内存）。
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
        // syscall (0f 05)
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
            &[0x0f, 0x05][..],
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
        // pushfq (9c)；popfq (9d)；mov ss, rax (8e d0)
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
        // 对照：mov ds, eax（8e d8，段编码 3=DS 非 SS）不抑制 #DB，可接受
        assert!(analyze_insn(&[0x8e, 0xd8]).is_ok());
        // 对照：普通 mov（非 SS 目标）可接受
        assert!(analyze_insn(&[0x48, 0x89, 0xe5]).is_ok());
        // Intel 明确规定 LSS 不具有 MOV SS/POP SS 的事件抑制行为；Linux 的
        // insn_masking_exception() 也不拒绝它，不能在这里扩大拒绝范围。
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

        // new_disp = 0 + (0x1000 - 0x2000) = -0x1000；opcode/ModRM 不变，仅 disp 被改写。
        assert_eq!(&slot[..3], &insn[..3]);
        assert_eq!(&slot[3..7], &(-0x1000i32).to_le_bytes());

        // 从 slot 执行后仍指向原有效地址：slot+7+new_disp == probe+7+0
        let new_disp = i32::from_le_bytes([slot[3], slot[4], slot[5], slot[6]]);
        let eff = slot_vaddr as i64 + 7 + new_disp as i64;
        assert_eq!(eff, probe_vaddr as i64 + 7);
    }

    #[test]
    fn build_slot_displacement_overflow() {
        let insn: [u8; 7] = [0x48, 0x8d, 0x05, 0x00, 0x00, 0x00, 0x00];
        let a = analyze_insn(&insn).unwrap();
        let mut slot = [0u8; 16];
        // 跨度超过 i32 范围 -> 重定位失败。
        let huge = (i32::MAX as usize) + 2;
        let err = build_xol_slot(&a, huge, 0, &insn, &mut slot).unwrap_err();
        assert_eq!(err, UprobeInsnError::DisplacementOverflow);
    }

    #[test]
    fn analyze_errors() {
        // 空输入。
        assert_eq!(analyze_insn(&[]).unwrap_err(), UprobeInsnError::Truncated);
        // 不完整指令（call rel32 只有 opcode、缺 4 字节 imm）-> 解码耗尽输入。
        assert_eq!(
            analyze_insn(&[0xe8]).unwrap_err(),
            UprobeInsnError::Truncated
        );
        // 非法操作码（push es 在 64 位长模式下无效）。
        assert_eq!(
            analyze_insn(&[0x06]).unwrap_err(),
            UprobeInsnError::DecodeFailed
        );
    }
}
