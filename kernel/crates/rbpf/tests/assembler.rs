// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2017 Rich Lane <lanerl@gmail.com>

#![allow(clippy::unreadable_literal)]

extern crate rbpf;
mod common;

use common::{TCP_SACK_ASM, TCP_SACK_BIN};
use rbpf::{assembler::assemble, ebpf};

fn asm(src: &str) -> Result<Vec<ebpf::Insn>, String> {
    Ok(ebpf::to_insn_vec(&(assemble(src))?))
}

fn insn(opc: u8, dst: u8, src: u8, off: i16, imm: i32) -> ebpf::Insn {
    ebpf::Insn {
        opc,
        dst,
        src,
        off,
        imm,
    }
}

#[test]
fn test_empty() {
    assert_eq!(asm(""), Ok(vec![]));
}

// Example for InstructionType::NoOperand.
#[test]
fn test_exit() {
    assert_eq!(asm("exit"), Ok(vec![insn(ebpf::EXIT, 0, 0, 0, 0)]));
}

// Example for InstructionType::AluBinary.
#[test]
fn test_add64() {
    assert_eq!(
        asm("add64 r1, r3"),
        Ok(vec![insn(ebpf::ADD64_REG, 1, 3, 0, 0)])
    );
    assert_eq!(
        asm("add64 r1, 5"),
        Ok(vec![insn(ebpf::ADD64_IMM, 1, 0, 0, 5)])
    );
}

// Example for InstructionType::AluUnary.
#[test]
fn test_neg64() {
    assert_eq!(asm("neg64 r1"), Ok(vec![insn(ebpf::NEG64, 1, 0, 0, 0)]));
}

// Example for InstructionType::LoadReg.
#[test]
fn test_ldxw() {
    assert_eq!(
        asm("ldxw r1, [r2+5]"),
        Ok(vec![insn(ebpf::LD_W_REG, 1, 2, 5, 0)])
    );
}

// Example for InstructionType::StoreImm.
#[test]
fn test_stw() {
    assert_eq!(
        asm("stw [r2+5], 7"),
        Ok(vec![insn(ebpf::ST_W_IMM, 2, 0, 5, 7)])
    );
}

// Example for InstructionType::StoreReg.
#[test]
fn test_stxw() {
    assert_eq!(
        asm("stxw [r2+5], r8"),
        Ok(vec![insn(ebpf::ST_W_REG, 2, 8, 5, 0)])
    );
}

// Example for InstructionType::JumpUnconditional.
#[test]
fn test_ja() {
    assert_eq!(asm("ja +8"), Ok(vec![insn(ebpf::JA, 0, 0, 8, 0)]));
    assert_eq!(asm("ja -3"), Ok(vec![insn(ebpf::JA, 0, 0, -3, 0)]));
}

// Example for InstructionType::JumpConditional.
#[test]
fn test_jeq() {
    assert_eq!(
        asm("jeq r1, 4, +8"),
        Ok(vec![insn(ebpf::JEQ_IMM, 1, 0, 8, 4)])
    );
    assert_eq!(
        asm("jeq r1, r3, +8"),
        Ok(vec![insn(ebpf::JEQ_REG, 1, 3, 8, 0)])
    );
}

// Example for InstructionType::Call.
#[test]
fn test_call() {
    assert_eq!(asm("call 300"), Ok(vec![insn(ebpf::CALL, 0, 0, 0, 300)]));
}

// Example for InstructionType::Endian.
#[test]
fn test_be32() {
    assert_eq!(asm("be32 r1"), Ok(vec![insn(ebpf::BE, 1, 0, 0, 32)]));
}

// Example for InstructionType::LoadImm.
#[test]
fn test_lddw() {
    assert_eq!(
        asm("lddw r1, 0x1234abcd5678eeff"),
        Ok(vec![
            insn(ebpf::LD_DW_IMM, 1, 0, 0, 0x5678eeff),
            insn(0, 0, 0, 0, 0x1234abcd)
        ])
    );
    assert_eq!(
        asm("lddw r1, 0xff11ee22dd33cc44"),
        Ok(vec![
            insn(ebpf::LD_DW_IMM, 1, 0, 0, 0xdd33cc44u32 as i32),
            insn(0, 0, 0, 0, 0xff11ee22u32 as i32)
        ])
    );
}

// Example for InstructionType::LoadAbs.
#[test]
fn test_ldabsw() {
    assert_eq!(asm("ldabsw 1"), Ok(vec![insn(ebpf::LD_ABS_W, 0, 0, 0, 1)]));
}

// Example for InstructionType::LoadInd.
#[test]
fn test_ldindw() {
    assert_eq!(
        asm("ldindw r1, 2"),
        Ok(vec![insn(ebpf::LD_IND_W, 0, 1, 0, 2)])
    );
}

// Example for InstructionType::LoadReg.
#[test]
fn test_ldxdw() {
    assert_eq!(
        asm("ldxdw r1, [r2+3]"),
        Ok(vec![insn(ebpf::LD_DW_REG, 1, 2, 3, 0)])
    );
}

// Example for InstructionType::StoreImm.
#[test]
fn test_sth() {
    assert_eq!(
        asm("sth [r1+2], 3"),
        Ok(vec![insn(ebpf::ST_H_IMM, 1, 0, 2, 3)])
    );
}

// Example for InstructionType::StoreReg.
#[test]
fn test_stxh() {
    assert_eq!(
        asm("stxh [r1+2], r3"),
        Ok(vec![insn(ebpf::ST_H_REG, 1, 3, 2, 0)])
    );
}

// Test all supported AluBinary mnemonics.
#[test]
fn test_alu_binary() {
    assert_eq!(
        asm("add r1, r2
                    sub r1, r2
                    mul r1, r2
                    div r1, r2
                    or r1, r2
                    and r1, r2
                    lsh r1, r2
                    rsh r1, r2
                    mod r1, r2
                    xor r1, r2
                    mov r1, r2
                    arsh r1, r2"),
        Ok(vec![
            insn(ebpf::ADD64_REG, 1, 2, 0, 0),
            insn(ebpf::SUB64_REG, 1, 2, 0, 0),
            insn(ebpf::MUL64_REG, 1, 2, 0, 0),
            insn(ebpf::DIV64_REG, 1, 2, 0, 0),
            insn(ebpf::OR64_REG, 1, 2, 0, 0),
            insn(ebpf::AND64_REG, 1, 2, 0, 0),
            insn(ebpf::LSH64_REG, 1, 2, 0, 0),
            insn(ebpf::RSH64_REG, 1, 2, 0, 0),
            insn(ebpf::MOD64_REG, 1, 2, 0, 0),
            insn(ebpf::XOR64_REG, 1, 2, 0, 0),
            insn(ebpf::MOV64_REG, 1, 2, 0, 0),
            insn(ebpf::ARSH64_REG, 1, 2, 0, 0)
        ])
    );

    assert_eq!(
        asm("add r1, 2
                    sub r1, 2
                    mul r1, 2
                    div r1, 2
                    or r1, 2
                    and r1, 2
                    lsh r1, 2
                    rsh r1, 2
                    mod r1, 2
                    xor r1, 2
                    mov r1, 2
                    arsh r1, 2"),
        Ok(vec![
            insn(ebpf::ADD64_IMM, 1, 0, 0, 2),
            insn(ebpf::SUB64_IMM, 1, 0, 0, 2),
            insn(ebpf::MUL64_IMM, 1, 0, 0, 2),
            insn(ebpf::DIV64_IMM, 1, 0, 0, 2),
            insn(ebpf::OR64_IMM, 1, 0, 0, 2),
            insn(ebpf::AND64_IMM, 1, 0, 0, 2),
            insn(ebpf::LSH64_IMM, 1, 0, 0, 2),
            insn(ebpf::RSH64_IMM, 1, 0, 0, 2),
            insn(ebpf::MOD64_IMM, 1, 0, 0, 2),
            insn(ebpf::XOR64_IMM, 1, 0, 0, 2),
            insn(ebpf::MOV64_IMM, 1, 0, 0, 2),
            insn(ebpf::ARSH64_IMM, 1, 0, 0, 2)
        ])
    );

    assert_eq!(
        asm("add64 r1, r2
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
                    arsh64 r1, r2"),
        Ok(vec![
            insn(ebpf::ADD64_REG, 1, 2, 0, 0),
            insn(ebpf::SUB64_REG, 1, 2, 0, 0),
            insn(ebpf::MUL64_REG, 1, 2, 0, 0),
            insn(ebpf::DIV64_REG, 1, 2, 0, 0),
            insn(ebpf::OR64_REG, 1, 2, 0, 0),
            insn(ebpf::AND64_REG, 1, 2, 0, 0),
            insn(ebpf::LSH64_REG, 1, 2, 0, 0),
            insn(ebpf::RSH64_REG, 1, 2, 0, 0),
            insn(ebpf::MOD64_REG, 1, 2, 0, 0),
            insn(ebpf::XOR64_REG, 1, 2, 0, 0),
            insn(ebpf::MOV64_REG, 1, 2, 0, 0),
            insn(ebpf::ARSH64_REG, 1, 2, 0, 0)
        ])
    );

    assert_eq!(
        asm("add64 r1, 2
                    sub64 r1, 2
                    mul64 r1, 2
                    div64 r1, 2
                    or64 r1, 2
                    and64 r1, 2
                    lsh64 r1, 2
                    rsh64 r1, 2
                    mod64 r1, 2
                    xor64 r1, 2
                    mov64 r1, 2
                    arsh64 r1, 2"),
        Ok(vec![
            insn(ebpf::ADD64_IMM, 1, 0, 0, 2),
            insn(ebpf::SUB64_IMM, 1, 0, 0, 2),
            insn(ebpf::MUL64_IMM, 1, 0, 0, 2),
            insn(ebpf::DIV64_IMM, 1, 0, 0, 2),
            insn(ebpf::OR64_IMM, 1, 0, 0, 2),
            insn(ebpf::AND64_IMM, 1, 0, 0, 2),
            insn(ebpf::LSH64_IMM, 1, 0, 0, 2),
            insn(ebpf::RSH64_IMM, 1, 0, 0, 2),
            insn(ebpf::MOD64_IMM, 1, 0, 0, 2),
            insn(ebpf::XOR64_IMM, 1, 0, 0, 2),
            insn(ebpf::MOV64_IMM, 1, 0, 0, 2),
            insn(ebpf::ARSH64_IMM, 1, 0, 0, 2)
        ])
    );

    assert_eq!(
        asm("add32 r1, r2
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
                    arsh32 r1, r2"),
        Ok(vec![
            insn(ebpf::ADD32_REG, 1, 2, 0, 0),
            insn(ebpf::SUB32_REG, 1, 2, 0, 0),
            insn(ebpf::MUL32_REG, 1, 2, 0, 0),
            insn(ebpf::DIV32_REG, 1, 2, 0, 0),
            insn(ebpf::OR32_REG, 1, 2, 0, 0),
            insn(ebpf::AND32_REG, 1, 2, 0, 0),
            insn(ebpf::LSH32_REG, 1, 2, 0, 0),
            insn(ebpf::RSH32_REG, 1, 2, 0, 0),
            insn(ebpf::MOD32_REG, 1, 2, 0, 0),
            insn(ebpf::XOR32_REG, 1, 2, 0, 0),
            insn(ebpf::MOV32_REG, 1, 2, 0, 0),
            insn(ebpf::ARSH32_REG, 1, 2, 0, 0)
        ])
    );

    assert_eq!(
        asm("add32 r1, 2
                    sub32 r1, 2
                    mul32 r1, 2
                    div32 r1, 2
                    or32 r1, 2
                    and32 r1, 2
                    lsh32 r1, 2
                    rsh32 r1, 2
                    mod32 r1, 2
                    xor32 r1, 2
                    mov32 r1, 2
                    arsh32 r1, 2"),
        Ok(vec![
            insn(ebpf::ADD32_IMM, 1, 0, 0, 2),
            insn(ebpf::SUB32_IMM, 1, 0, 0, 2),
            insn(ebpf::MUL32_IMM, 1, 0, 0, 2),
            insn(ebpf::DIV32_IMM, 1, 0, 0, 2),
            insn(ebpf::OR32_IMM, 1, 0, 0, 2),
            insn(ebpf::AND32_IMM, 1, 0, 0, 2),
            insn(ebpf::LSH32_IMM, 1, 0, 0, 2),
            insn(ebpf::RSH32_IMM, 1, 0, 0, 2),
            insn(ebpf::MOD32_IMM, 1, 0, 0, 2),
            insn(ebpf::XOR32_IMM, 1, 0, 0, 2),
            insn(ebpf::MOV32_IMM, 1, 0, 0, 2),
            insn(ebpf::ARSH32_IMM, 1, 0, 0, 2)
        ])
    );
}

// Test all supported AluUnary mnemonics.
#[test]
fn test_alu_unary() {
    assert_eq!(
        asm("neg r1
                    neg64 r1
                    neg32 r1"),
        Ok(vec![
            insn(ebpf::NEG64, 1, 0, 0, 0),
            insn(ebpf::NEG64, 1, 0, 0, 0),
            insn(ebpf::NEG32, 1, 0, 0, 0)
        ])
    );
}

// Test all supported LoadAbs mnemonics.
#[test]
fn test_load_abs() {
    assert_eq!(
        asm("ldabsw 1
                    ldabsh 1
                    ldabsb 1
                    ldabsdw 1"),
        Ok(vec![
            insn(ebpf::LD_ABS_W, 0, 0, 0, 1),
            insn(ebpf::LD_ABS_H, 0, 0, 0, 1),
            insn(ebpf::LD_ABS_B, 0, 0, 0, 1),
            insn(ebpf::LD_ABS_DW, 0, 0, 0, 1)
        ])
    );
}

// Test all supported LoadInd mnemonics.
#[test]
fn test_load_ind() {
    assert_eq!(
        asm("ldindw r1, 2
                    ldindh r1, 2
                    ldindb r1, 2
                    ldinddw r1, 2"),
        Ok(vec![
            insn(ebpf::LD_IND_W, 0, 1, 0, 2),
            insn(ebpf::LD_IND_H, 0, 1, 0, 2),
            insn(ebpf::LD_IND_B, 0, 1, 0, 2),
            insn(ebpf::LD_IND_DW, 0, 1, 0, 2)
        ])
    );
}

// Test all supported LoadReg mnemonics.
#[test]
fn test_load_reg() {
    assert_eq!(
        asm("ldxw r1, [r2+3]
                    ldxh r1, [r2+3]
                    ldxb r1, [r2+3]
                    ldxdw r1, [r2+3]"),
        Ok(vec![
            insn(ebpf::LD_W_REG, 1, 2, 3, 0),
            insn(ebpf::LD_H_REG, 1, 2, 3, 0),
            insn(ebpf::LD_B_REG, 1, 2, 3, 0),
            insn(ebpf::LD_DW_REG, 1, 2, 3, 0)
        ])
    );
}

// Test all supported StoreImm mnemonics.
#[test]
fn test_store_imm() {
    assert_eq!(
        asm("stw [r1+2], 3
                    sth [r1+2], 3
                    stb [r1+2], 3
                    stdw [r1+2], 3"),
        Ok(vec![
            insn(ebpf::ST_W_IMM, 1, 0, 2, 3),
            insn(ebpf::ST_H_IMM, 1, 0, 2, 3),
            insn(ebpf::ST_B_IMM, 1, 0, 2, 3),
            insn(ebpf::ST_DW_IMM, 1, 0, 2, 3)
        ])
    );
}

// Test all supported StoreReg mnemonics.
#[test]
fn test_store_reg() {
    assert_eq!(
        asm("stxw [r1+2], r3
                    stxh [r1+2], r3
                    stxb [r1+2], r3
                    stxdw [r1+2], r3"),
        Ok(vec![
            insn(ebpf::ST_W_REG, 1, 3, 2, 0),
            insn(ebpf::ST_H_REG, 1, 3, 2, 0),
            insn(ebpf::ST_B_REG, 1, 3, 2, 0),
            insn(ebpf::ST_DW_REG, 1, 3, 2, 0)
        ])
    );
}

// Test all supported JumpConditional mnemonics.
#[test]
fn test_jump_conditional() {
    assert_eq!(
        asm("jeq r1, r2, +3
                    jgt r1, r2, +3
                    jge r1, r2, +3
                    jlt r1, r2, +3
                    jle r1, r2, +3
                    jset r1, r2, +3
                    jne r1, r2, +3
                    jsgt r1, r2, +3
                    jsge r1, r2, +3
                    jslt r1, r2, +3
                    jsle r1, r2, +3"),
        Ok(vec![
            insn(ebpf::JEQ_REG, 1, 2, 3, 0),
            insn(ebpf::JGT_REG, 1, 2, 3, 0),
            insn(ebpf::JGE_REG, 1, 2, 3, 0),
            insn(ebpf::JLT_REG, 1, 2, 3, 0),
            insn(ebpf::JLE_REG, 1, 2, 3, 0),
            insn(ebpf::JSET_REG, 1, 2, 3, 0),
            insn(ebpf::JNE_REG, 1, 2, 3, 0),
            insn(ebpf::JSGT_REG, 1, 2, 3, 0),
            insn(ebpf::JSGE_REG, 1, 2, 3, 0),
            insn(ebpf::JSLT_REG, 1, 2, 3, 0),
            insn(ebpf::JSLE_REG, 1, 2, 3, 0)
        ])
    );

    assert_eq!(
        asm("jeq r1, 2, +3
                    jgt r1, 2, +3
                    jge r1, 2, +3
                    jlt r1, 2, +3
                    jle r1, 2, +3
                    jset r1, 2, +3
                    jne r1, 2, +3
                    jsgt r1, 2, +3
                    jsge r1, 2, +3
                    jslt r1, 2, +3
                    jsle r1, 2, +3"),
        Ok(vec![
            insn(ebpf::JEQ_IMM, 1, 0, 3, 2),
            insn(ebpf::JGT_IMM, 1, 0, 3, 2),
            insn(ebpf::JGE_IMM, 1, 0, 3, 2),
            insn(ebpf::JLT_IMM, 1, 0, 3, 2),
            insn(ebpf::JLE_IMM, 1, 0, 3, 2),
            insn(ebpf::JSET_IMM, 1, 0, 3, 2),
            insn(ebpf::JNE_IMM, 1, 0, 3, 2),
            insn(ebpf::JSGT_IMM, 1, 0, 3, 2),
            insn(ebpf::JSGE_IMM, 1, 0, 3, 2),
            insn(ebpf::JSLT_IMM, 1, 0, 3, 2),
            insn(ebpf::JSLE_IMM, 1, 0, 3, 2)
        ])
    );

    assert_eq!(
        asm("jeq32 r1, r2, +3
                    jgt32 r1, r2, +3
                    jge32 r1, r2, +3
                    jlt32 r1, r2, +3
                    jle32 r1, r2, +3
                    jset32 r1, r2, +3
                    jne32 r1, r2, +3
                    jsgt32 r1, r2, +3
                    jsge32 r1, r2, +3
                    jslt32 r1, r2, +3
                    jsle32 r1, r2, +3"),
        Ok(vec![
            insn(ebpf::JEQ_REG32, 1, 2, 3, 0),
            insn(ebpf::JGT_REG32, 1, 2, 3, 0),
            insn(ebpf::JGE_REG32, 1, 2, 3, 0),
            insn(ebpf::JLT_REG32, 1, 2, 3, 0),
            insn(ebpf::JLE_REG32, 1, 2, 3, 0),
            insn(ebpf::JSET_REG32, 1, 2, 3, 0),
            insn(ebpf::JNE_REG32, 1, 2, 3, 0),
            insn(ebpf::JSGT_REG32, 1, 2, 3, 0),
            insn(ebpf::JSGE_REG32, 1, 2, 3, 0),
            insn(ebpf::JSLT_REG32, 1, 2, 3, 0),
            insn(ebpf::JSLE_REG32, 1, 2, 3, 0)
        ])
    );

    assert_eq!(
        asm("jeq32 r1, 2, +3
                    jgt32 r1, 2, +3
                    jge32 r1, 2, +3
                    jlt32 r1, 2, +3
                    jle32 r1, 2, +3
                    jset32 r1, 2, +3
                    jne32 r1, 2, +3
                    jsgt32 r1, 2, +3
                    jsge32 r1, 2, +3
                    jslt32 r1, 2, +3
                    jsle32 r1, 2, +3"),
        Ok(vec![
            insn(ebpf::JEQ_IMM32, 1, 0, 3, 2),
            insn(ebpf::JGT_IMM32, 1, 0, 3, 2),
            insn(ebpf::JGE_IMM32, 1, 0, 3, 2),
            insn(ebpf::JLT_IMM32, 1, 0, 3, 2),
            insn(ebpf::JLE_IMM32, 1, 0, 3, 2),
            insn(ebpf::JSET_IMM32, 1, 0, 3, 2),
            insn(ebpf::JNE_IMM32, 1, 0, 3, 2),
            insn(ebpf::JSGT_IMM32, 1, 0, 3, 2),
            insn(ebpf::JSGE_IMM32, 1, 0, 3, 2),
            insn(ebpf::JSLT_IMM32, 1, 0, 3, 2),
            insn(ebpf::JSLE_IMM32, 1, 0, 3, 2)
        ])
    );
}

// Test all supported Endian mnemonics.
#[test]
fn test_endian() {
    assert_eq!(
        asm("be16 r1
                    be32 r1
                    be64 r1
                    le16 r1
                    le32 r1
                    le64 r1"),
        Ok(vec![
            insn(ebpf::BE, 1, 0, 0, 16),
            insn(ebpf::BE, 1, 0, 0, 32),
            insn(ebpf::BE, 1, 0, 0, 64),
            insn(ebpf::LE, 1, 0, 0, 16),
            insn(ebpf::LE, 1, 0, 0, 32),
            insn(ebpf::LE, 1, 0, 0, 64)
        ])
    );
}

#[test]
fn test_large_immediate() {
    assert_eq!(
        asm("add64 r1, 2147483647"),
        Ok(vec![insn(ebpf::ADD64_IMM, 1, 0, 0, 2147483647)])
    );
    assert_eq!(
        asm("add64 r1, -2147483648"),
        Ok(vec![insn(ebpf::ADD64_IMM, 1, 0, 0, -2147483648)])
    );
}

#[test]
fn test_tcp_sack() {
    assert_eq!(assemble(TCP_SACK_ASM), Ok(TCP_SACK_BIN.to_vec()));
}

#[test]
fn test_error_invalid_instruction() {
    assert_eq!(asm("abcd"), Err("Invalid instruction \"abcd\"".to_string()));
}

#[test]
fn test_error_unexpected_operands() {
    assert_eq!(
        asm("add 1, 2"),
        Err("Failed to encode add: Unexpected operands: [Integer(1), Integer(2)]".to_string())
    );
}

#[test]
fn test_error_too_many_operands() {
    assert_eq!(
        asm("add 1, 2, 3, 4"),
        Err("Failed to encode add: Too many operands".to_string())
    );
}

#[test]
fn test_error_operands_out_of_range() {
    assert_eq!(
        asm("add r16, r2"),
        Err("Failed to encode add: Invalid destination register 16".to_string())
    );
    assert_eq!(
        asm("add r1, r16"),
        Err("Failed to encode add: Invalid source register 16".to_string())
    );
    assert_eq!(
        asm("ja -32769"),
        Err("Failed to encode ja: Invalid offset -32769".to_string())
    );
    assert_eq!(
        asm("ja 32768"),
        Err("Failed to encode ja: Invalid offset 32768".to_string())
    );
    assert_eq!(
        asm("add r1, 4294967296"),
        Err("Failed to encode add: Invalid immediate 4294967296".to_string())
    );
    assert_eq!(
        asm("add r1, 2147483648"),
        Err("Failed to encode add: Invalid immediate 2147483648".to_string())
    );
    assert_eq!(
        asm("add r1, -2147483649"),
        Err("Failed to encode add: Invalid immediate -2147483649".to_string())
    );
}
