// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Copyright 2017 Rich Lane <lanerl@gmail.com>

//! This module translates eBPF assembly language to binary.

use alloc::{
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec,
    vec::Vec,
};

use self::InstructionType::{
    AluBinary, AluUnary, Call, Endian, JumpConditional, JumpUnconditional, LoadAbs, LoadImm,
    LoadInd, LoadReg, NoOperand, StoreImm, StoreReg,
};
use crate::{
    asm_parser::{
        parse, Instruction, Operand,
        Operand::{Integer, Memory, Nil, Register},
    },
    ebpf::{self, Insn},
};

#[derive(Clone, Copy, Debug, PartialEq)]
enum InstructionType {
    AluBinary,
    AluUnary,
    LoadImm,
    LoadAbs,
    LoadInd,
    LoadReg,
    StoreImm,
    StoreReg,
    JumpUnconditional,
    JumpConditional,
    Call,
    Endian(i64),
    NoOperand,
}

fn make_instruction_map() -> BTreeMap<String, (InstructionType, u8)> {
    let mut result = BTreeMap::new();

    let alu_binary_ops = [
        ("add", ebpf::BPF_ADD),
        ("sub", ebpf::BPF_SUB),
        ("mul", ebpf::BPF_MUL),
        ("div", ebpf::BPF_DIV),
        ("or", ebpf::BPF_OR),
        ("and", ebpf::BPF_AND),
        ("lsh", ebpf::BPF_LSH),
        ("rsh", ebpf::BPF_RSH),
        ("mod", ebpf::BPF_MOD),
        ("xor", ebpf::BPF_XOR),
        ("mov", ebpf::BPF_MOV),
        ("arsh", ebpf::BPF_ARSH),
    ];

    let mem_sizes = [
        ("w", ebpf::BPF_W),
        ("h", ebpf::BPF_H),
        ("b", ebpf::BPF_B),
        ("dw", ebpf::BPF_DW),
    ];

    let jump_conditions = [
        ("jeq", ebpf::BPF_JEQ),
        ("jgt", ebpf::BPF_JGT),
        ("jge", ebpf::BPF_JGE),
        ("jlt", ebpf::BPF_JLT),
        ("jle", ebpf::BPF_JLE),
        ("jset", ebpf::BPF_JSET),
        ("jne", ebpf::BPF_JNE),
        ("jsgt", ebpf::BPF_JSGT),
        ("jsge", ebpf::BPF_JSGE),
        ("jslt", ebpf::BPF_JSLT),
        ("jsle", ebpf::BPF_JSLE),
    ];

    {
        let mut entry = |name: &str, inst_type: InstructionType, opc: u8| {
            result.insert(name.to_string(), (inst_type, opc))
        };

        // Miscellaneous.
        entry("exit", NoOperand, ebpf::EXIT);
        entry("ja", JumpUnconditional, ebpf::JA);
        entry("call", Call, ebpf::CALL);
        entry("lddw", LoadImm, ebpf::LD_DW_IMM);

        // AluUnary.
        entry("neg", AluUnary, ebpf::NEG64);
        entry("neg32", AluUnary, ebpf::NEG32);
        entry("neg64", AluUnary, ebpf::NEG64);

        // AluBinary.
        for &(name, opc) in &alu_binary_ops {
            entry(name, AluBinary, ebpf::BPF_ALU64 | opc);
            entry(&format!("{name}32"), AluBinary, ebpf::BPF_ALU | opc);
            entry(&format!("{name}64"), AluBinary, ebpf::BPF_ALU64 | opc);
        }

        // LoadAbs, LoadInd, LoadReg, StoreImm, and StoreReg.
        for &(suffix, size) in &mem_sizes {
            entry(
                &format!("ldabs{suffix}"),
                LoadAbs,
                ebpf::BPF_ABS | ebpf::BPF_LD | size,
            );
            entry(
                &format!("ldind{suffix}"),
                LoadInd,
                ebpf::BPF_IND | ebpf::BPF_LD | size,
            );
            entry(
                &format!("ldx{suffix}"),
                LoadReg,
                ebpf::BPF_MEM | ebpf::BPF_LDX | size,
            );
            entry(
                &format!("st{suffix}"),
                StoreImm,
                ebpf::BPF_MEM | ebpf::BPF_ST | size,
            );
            entry(
                &format!("stx{suffix}"),
                StoreReg,
                ebpf::BPF_MEM | ebpf::BPF_STX | size,
            );
        }

        // JumpConditional.
        for &(name, condition) in &jump_conditions {
            entry(name, JumpConditional, ebpf::BPF_JMP | condition);
            entry(
                &format!("{name}32"),
                JumpConditional,
                ebpf::BPF_JMP32 | condition,
            );
        }

        // Endian.
        for &size in &[16, 32, 64] {
            entry(&format!("be{size}"), Endian(size), ebpf::BE);
            entry(&format!("le{size}"), Endian(size), ebpf::LE);
        }
    }

    result
}

fn insn(opc: u8, dst: i64, src: i64, off: i64, imm: i64) -> Result<Insn, String> {
    if !(0..16).contains(&dst) {
        return Err(format!("Invalid destination register {dst}"));
    }
    if dst < 0 || src >= 16 {
        return Err(format!("Invalid source register {src}"));
    }
    if !(-32768..32768).contains(&off) {
        return Err(format!("Invalid offset {off}"));
    }
    if !(-2147483648..2147483648).contains(&imm) {
        return Err(format!("Invalid immediate {imm}"));
    }
    Ok(Insn {
        opc,
        dst: dst as u8,
        src: src as u8,
        off: off as i16,
        imm: imm as i32,
    })
}

// TODO Use slice patterns when available and remove this function.
fn operands_tuple(operands: &[Operand]) -> Result<(Operand, Operand, Operand), String> {
    match operands.len() {
        0 => Ok((Nil, Nil, Nil)),
        1 => Ok((operands[0], Nil, Nil)),
        2 => Ok((operands[0], operands[1], Nil)),
        3 => Ok((operands[0], operands[1], operands[2])),
        _ => Err("Too many operands".to_string()),
    }
}

fn encode(inst_type: InstructionType, opc: u8, operands: &[Operand]) -> Result<Insn, String> {
    let (a, b, c) = (operands_tuple(operands))?;
    match (inst_type, a, b, c) {
        (AluBinary, Register(dst), Register(src), Nil) => insn(opc | ebpf::BPF_X, dst, src, 0, 0),
        (AluBinary, Register(dst), Integer(imm), Nil) => insn(opc | ebpf::BPF_K, dst, 0, 0, imm),
        (AluUnary, Register(dst), Nil, Nil) => insn(opc, dst, 0, 0, 0),
        (LoadAbs, Integer(imm), Nil, Nil) => insn(opc, 0, 0, 0, imm),
        (LoadInd, Register(src), Integer(imm), Nil) => insn(opc, 0, src, 0, imm),
        (LoadReg, Register(dst), Memory(src, off), Nil)
        | (StoreReg, Memory(dst, off), Register(src), Nil) => insn(opc, dst, src, off, 0),
        (StoreImm, Memory(dst, off), Integer(imm), Nil) => insn(opc, dst, 0, off, imm),
        (NoOperand, Nil, Nil, Nil) => insn(opc, 0, 0, 0, 0),
        (JumpUnconditional, Integer(off), Nil, Nil) => insn(opc, 0, 0, off, 0),
        (JumpConditional, Register(dst), Register(src), Integer(off)) => {
            insn(opc | ebpf::BPF_X, dst, src, off, 0)
        }
        (JumpConditional, Register(dst), Integer(imm), Integer(off)) => {
            insn(opc | ebpf::BPF_K, dst, 0, off, imm)
        }
        (Call, Integer(imm), Nil, Nil) => insn(opc, 0, 0, 0, imm),
        (Endian(size), Register(dst), Nil, Nil) => insn(opc, dst, 0, 0, size),
        (LoadImm, Register(dst), Integer(imm), Nil) => insn(opc, dst, 0, 0, (imm << 32) >> 32),
        _ => Err(format!("Unexpected operands: {operands:?}")),
    }
}

fn assemble_internal(parsed: &[Instruction]) -> Result<Vec<Insn>, String> {
    let instruction_map = make_instruction_map();
    let mut result: Vec<Insn> = vec![];
    for instruction in parsed {
        let name = instruction.name.as_str();
        match instruction_map.get(name) {
            Some(&(inst_type, opc)) => {
                match encode(inst_type, opc, &instruction.operands) {
                    Ok(insn) => result.push(insn),
                    Err(msg) => return Err(format!("Failed to encode {name}: {msg}")),
                }
                // Special case for lddw.
                if let LoadImm = inst_type {
                    if let Integer(imm) = instruction.operands[1] {
                        result.push(insn(0, 0, 0, 0, imm >> 32).unwrap());
                    }
                }
            }
            None => return Err(format!("Invalid instruction {name:?}")),
        }
    }
    Ok(result)
}

/// Parse assembly source and translate to binary.
///
/// # Examples
///
/// ```
/// use rbpf::assembler::assemble;
/// let prog = assemble("add64 r1, 0x605
///                      mov64 r2, 0x32
///                      mov64 r1, r0
///                      be16 r0
///                      neg64 r2
///                      exit");
/// println!("{:?}", prog);
/// # assert_eq!(prog,
/// #            Ok(vec![0x07, 0x01, 0x00, 0x00, 0x05, 0x06, 0x00, 0x00,
/// #                    0xb7, 0x02, 0x00, 0x00, 0x32, 0x00, 0x00, 0x00,
/// #                    0xbf, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
/// #                    0xdc, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
/// #                    0x87, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
/// #                    0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00]));
/// ```
///
/// This will produce the following output:
///
/// ```test
/// Ok([0x07, 0x01, 0x00, 0x00, 0x05, 0x06, 0x00, 0x00,
///     0xb7, 0x02, 0x00, 0x00, 0x32, 0x00, 0x00, 0x00,
///     0xbf, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
///     0xdc, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00,
///     0x87, 0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
///     0x95, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00])
/// ```
pub fn assemble(src: &str) -> Result<Vec<u8>, String> {
    let parsed = (parse(src))?;
    let insns = (assemble_internal(&parsed))?;
    let mut result: Vec<u8> = vec![];
    for insn in insns {
        result.extend_from_slice(&insn.to_array());
    }
    Ok(result)
}
