// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2017 Jan-Erik Rediger <badboy@archlinux.us>
//
// Adopted from tests in `tests/assembler.rs`

extern crate rbpf;
mod common;

use rbpf::{assembler::assemble, disassembler::to_insn_vec};

// Using a macro to keep actual line numbers in failure output
macro_rules! disasm {
    ($src:expr) => {{
        let src = $src;
        let asm = assemble(src).expect("Can't assemble from string");
        let insn = to_insn_vec(&asm);
        let reasm = insn
            .into_iter()
            .map(|ins| ins.desc)
            .collect::<Vec<_>>()
            .join("\n");

        assert_eq!(src, reasm);
    }};
}

#[test]
fn test_empty() {
    disasm!("");
}

// Example for InstructionType::NoOperand.
#[test]
fn test_exit() {
    disasm!("exit");
}

// Example for InstructionType::AluBinary.
#[test]
fn test_add64() {
    disasm!("add64 r1, r3");
    disasm!("add64 r1, 0x5");
}

// Example for InstructionType::AluUnary.
#[test]
fn test_neg64() {
    disasm!("neg64 r1");
}

// Example for InstructionType::LoadReg.
#[test]
fn test_ldxw() {
    disasm!("ldxw r1, [r2+0x5]");
}

// Example for InstructionType::StoreImm.
#[test]
fn test_stw() {
    disasm!("stw [r2+0x5], 0x7");
}

// Example for InstructionType::StoreReg.
#[test]
fn test_stxw() {
    disasm!("stxw [r2+0x5], r8");
}

// Example for InstructionType::JumpUnconditional.
#[test]
fn test_ja() {
    disasm!("ja +0x8");
}

// Example for InstructionType::JumpConditional.
#[test]
fn test_jeq() {
    disasm!("jeq r1, 0x4, +0x8");
    disasm!("jeq r1, r3, +0x8");
}

// Example for InstructionType::Call.
#[test]
fn test_call() {
    disasm!("call 0x3");
}

// Example for InstructionType::Endian.
#[test]
fn test_be32() {
    disasm!("be32 r1");
}

// Example for InstructionType::LoadImm.
#[test]
fn test_lddw() {
    disasm!("lddw r1, 0x1234abcd5678eeff");
    disasm!("lddw r1, 0xff11ee22dd33cc44");
}

// Example for InstructionType::LoadAbs.
#[test]
fn test_ldabsw() {
    disasm!("ldabsw 0x1");
}

// Example for InstructionType::LoadInd.
#[test]
fn test_ldindw() {
    disasm!("ldindw r1, 0x2");
}

// Example for InstructionType::LoadReg.
#[test]
fn test_ldxdw() {
    disasm!("ldxdw r1, [r2+0x3]");
}

// Example for InstructionType::StoreImm.
#[test]
fn test_sth() {
    disasm!("sth [r1+0x2], 0x3");
}

// Example for InstructionType::StoreReg.
#[test]
fn test_stxh() {
    disasm!("stxh [r1+0x2], r3");
}

// Test all supported AluBinary mnemonics.
#[test]
fn test_alu_binary() {
    disasm!(
        "add64 r1, r2
sub64 r1, r2
mul64 r1, r2
div64 r1, r2
or64 r1, r2
and64 r1, r2
lsh64 r1, r2
rsh64 r1, r2
mod64 r1, r2
xor64 r1, r2
mov64 r1, r2
arsh64 r1, r2"
    );

    disasm!(
        "add64 r1, 0x2
sub64 r1, 0x2
mul64 r1, 0x2
div64 r1, 0x2
or64 r1, 0x2
and64 r1, 0x2
lsh64 r1, 0x2
rsh64 r1, 0x2
mod64 r1, 0x2
xor64 r1, 0x2
mov64 r1, 0x2
arsh64 r1, 0x2"
    );

    disasm!(
        "add32 r1, r2
sub32 r1, r2
mul32 r1, r2
div32 r1, r2
or32 r1, r2
and32 r1, r2
lsh32 r1, r2
rsh32 r1, r2
mod32 r1, r2
xor32 r1, r2
mov32 r1, r2
arsh32 r1, r2"
    );

    disasm!(
        "add32 r1, 0x2
sub32 r1, 0x2
mul32 r1, 0x2
div32 r1, 0x2
or32 r1, 0x2
and32 r1, 0x2
lsh32 r1, 0x2
rsh32 r1, 0x2
mod32 r1, 0x2
xor32 r1, 0x2
mov32 r1, 0x2
arsh32 r1, 0x2"
    );
}

// Test all supported AluUnary mnemonics.
#[test]
fn test_alu_unary() {
    disasm!(
        "neg64 r1
neg32 r1"
    );
}

// Test all supported LoadAbs mnemonics.
#[test]
fn test_load_abs() {
    disasm!(
        "ldabsw 0x1
ldabsh 0x1
ldabsb 0x1
ldabsdw 0x1"
    );
}

// Test all supported LoadInd mnemonics.
#[test]
fn test_load_ind() {
    disasm!(
        "ldindw r1, 0x2
ldindh r1, 0x2
ldindb r1, 0x2
ldinddw r1, 0x2"
    );
}

// Test all supported LoadReg mnemonics.
#[test]
fn test_load_reg() {
    disasm!(
        r"ldxw r1, [r2+0x3]
ldxh r1, [r2+0x3]
ldxb r1, [r2+0x3]
ldxdw r1, [r2+0x3]"
    );
}

// Test all supported StoreImm mnemonics.
#[test]
fn test_store_imm() {
    disasm!(
        "stw [r1+0x2], 0x3
sth [r1+0x2], 0x3
stb [r1+0x2], 0x3
stdw [r1+0x2], 0x3"
    );
}

// Test all supported StoreReg mnemonics.
#[test]
fn test_store_reg() {
    disasm!(
        "stxw [r1+0x2], r3
stxh [r1+0x2], r3
stxb [r1+0x2], r3
stxdw [r1+0x2], r3"
    );
}

// Test all supported JumpConditional mnemonics.
#[test]
fn test_jump_conditional() {
    disasm!(
        "jeq r1, r2, +0x3
jgt r1, r2, +0x3
jge r1, r2, +0x3
jlt r1, r2, +0x3
jle r1, r2, +0x3
jset r1, r2, +0x3
jne r1, r2, +0x3
jsgt r1, r2, +0x3
jsge r1, r2, -0x3
jslt r1, r2, +0x3
jsle r1, r2, -0x3"
    );

    disasm!(
        "jeq r1, 0x2, +0x3
jgt r1, 0x2, +0x3
jge r1, 0x2, +0x3
jlt r1, 0x2, +0x3
jle r1, 0x2, +0x3
jset r1, 0x2, +0x3
jne r1, 0x2, +0x3
jsgt r1, 0x2, +0x3
jsge r1, 0x2, -0x3
jslt r1, 0x2, +0x3
jsle r1, 0x2, -0x3"
    );

    disasm!(
        "jeq32 r1, r2, +0x3
jgt32 r1, r2, +0x3
jge32 r1, r2, +0x3
jlt32 r1, r2, +0x3
jle32 r1, r2, +0x3
jset32 r1, r2, +0x3
jne32 r1, r2, +0x3
jsgt32 r1, r2, +0x3
jsge32 r1, r2, -0x3
jslt32 r1, r2, +0x3
jsle32 r1, r2, -0x3"
    );

    disasm!(
        "jeq32 r1, 0x2, +0x3
jgt32 r1, 0x2, +0x3
jge32 r1, 0x2, +0x3
jlt32 r1, 0x2, +0x3
jle32 r1, 0x2, +0x3
jset32 r1, 0x2, +0x3
jne32 r1, 0x2, +0x3
jsgt32 r1, 0x2, +0x3
jsge32 r1, 0x2, -0x3
jslt32 r1, 0x2, +0x3
jsle32 r1, 0x2, -0x3"
    );
}

// Test all supported Endian mnemonics.
#[test]
fn test_endian() {
    disasm!(
        "be16 r1
be32 r1
be64 r1
le16 r1
le32 r1
le64 r1"
    );
}

#[test]
fn test_large_immediate() {
    disasm!("add64 r1, 0x7fffffff");
    disasm!("add64 r1, 0x7fffffff");
}

// Non-regression tests for overflow when trying to negate offset 0x8000i16.
#[test]
fn test_offset_overflow() {
    let insns = [
        0x62, 0x01, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, // stw
        0x6a, 0x01, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, // sth
        0x72, 0x01, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, // stb
        0x7a, 0x01, 0x00, 0x80, 0x01, 0x00, 0x00, 0x00, // stdw
        0x61, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // ldxw
        0x69, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // ldxh
        0x71, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // ldxb
        0x79, 0x01, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // ldxdw
        0x15, 0x01, 0x00, 0x80, 0x02, 0x00, 0x00, 0x00, // jeq (imm)
        0x1d, 0x21, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // jeq (reg)
        0x16, 0x01, 0x00, 0x80, 0x02, 0x00, 0x00, 0x00, // jeq32 (imm)
        0x1e, 0x21, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, // jeq32 (reg)
    ];

    let expected_output = "stw [r1-0x8000], 0x1
sth [r1-0x8000], 0x1
stb [r1-0x8000], 0x1
stdw [r1-0x8000], 0x1
ldxw r1, [r0-0x8000]
ldxh r1, [r0-0x8000]
ldxb r1, [r0-0x8000]
ldxdw r1, [r0-0x8000]
jeq r1, 0x2, -0x8000
jeq r1, r2, -0x8000
jeq32 r1, 0x2, -0x8000
jeq32 r1, r2, -0x8000";

    let prog = to_insn_vec(&insns);
    let asm = prog
        .into_iter()
        .map(|ins| ins.desc)
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(asm, expected_output);
}
