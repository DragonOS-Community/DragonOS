//! A deliberately restricted, memory-safe executor for cgroup device programs.
//!
//! A program accepted here cannot access arbitrary memory, call helpers, use
//! maps or loop. This is separate from the raw-pointer VM used by other BPF
//! program types: a device filter is invoked from the VFS permission path and
//! must never turn an untrusted register into a kernel pointer.

use alloc::vec;
use alloc::vec::Vec;
use rbpf::ebpf;
use system_error::SystemError;

const INSN_SIZE: usize = 8;
const MAX_INSNS: usize = 8192;
const REGISTER_COUNT: usize = 11;

#[derive(Clone, Copy, Debug)]
pub struct DeviceAccess {
    pub access_type: u32,
    pub major: u32,
    pub minor: u32,
}

#[derive(Debug)]
pub struct DeviceProgram {
    instructions: Vec<Instruction>,
}

#[derive(Clone, Copy, Debug)]
enum Operand {
    Immediate(i32),
    Register(u8),
}

#[derive(Clone, Copy, Debug)]
enum Width {
    Bits32,
    Bits64,
}

#[derive(Clone, Copy, Debug)]
enum AluOp {
    Add,
    Sub,
    Mul,
    Or,
    And,
    Xor,
    LeftShift,
    RightShift,
    ArithmeticRightShift,
    Move,
    Negate,
}

#[derive(Clone, Copy, Debug)]
enum CompareOp {
    Equal,
    NotEqual,
    Greater,
    GreaterEqual,
    Less,
    LessEqual,
    SignedGreater,
    SignedGreaterEqual,
    SignedLess,
    SignedLessEqual,
    Set,
}

#[derive(Clone, Copy, Debug)]
enum Instruction {
    LoadContext {
        dst: u8,
        src: u8,
        field: u8,
    },
    Alu {
        width: Width,
        op: AluOp,
        dst: u8,
        src: Operand,
    },
    Jump {
        target: usize,
    },
    JumpIf {
        width: Width,
        op: CompareOp,
        dst: u8,
        src: Operand,
        target: usize,
    },
    Exit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RegisterKind {
    Uninitialized,
    Context,
    Scalar,
}

type State = [RegisterKind; REGISTER_COUNT];

impl DeviceProgram {
    /// Decode and verify all possible paths before the program becomes visible
    /// to a cgroup. Unknown instructions are rejected, never interpreted as a
    /// no-op or an implicit allow.
    pub fn verify(insns: &[u8]) -> Result<Self, SystemError> {
        if insns.is_empty() || !insns.len().is_multiple_of(INSN_SIZE) {
            return Err(SystemError::EINVAL);
        }
        let count = insns.len() / INSN_SIZE;
        if count > MAX_INSNS {
            return Err(SystemError::E2BIG);
        }

        let mut instructions = Vec::with_capacity(count);
        for (pc, raw) in insns.chunks_exact(INSN_SIZE).enumerate() {
            instructions.push(decode(raw, pc, count)?);
        }

        // All branch edges are strictly forward. A single increasing-PC pass
        // therefore sees every predecessor before checking a join point.
        let mut incoming: Vec<Option<State>> = vec![None; count];
        let mut initial = [RegisterKind::Uninitialized; REGISTER_COUNT];
        initial[1] = RegisterKind::Context;
        incoming[0] = Some(initial);

        for pc in 0..count {
            let Some(mut state) = incoming[pc] else {
                continue;
            };
            match instructions[pc] {
                Instruction::LoadContext { dst, src, .. } => {
                    if state[src as usize] != RegisterKind::Context {
                        return Err(SystemError::EINVAL);
                    }
                    state[dst as usize] = RegisterKind::Scalar;
                    propagate(&mut incoming, pc + 1, state)?;
                }
                Instruction::Alu { op, dst, src, .. } => {
                    if !matches!(op, AluOp::Move) && state[dst as usize] != RegisterKind::Scalar {
                        return Err(SystemError::EINVAL);
                    }
                    require_scalar(src, &state)?;
                    state[dst as usize] = RegisterKind::Scalar;
                    propagate(&mut incoming, pc + 1, state)?;
                }
                Instruction::Jump { target } => propagate(&mut incoming, target, state)?,
                Instruction::JumpIf {
                    dst, src, target, ..
                } => {
                    if state[dst as usize] != RegisterKind::Scalar {
                        return Err(SystemError::EINVAL);
                    }
                    require_scalar(src, &state)?;
                    propagate(&mut incoming, pc + 1, state)?;
                    propagate(&mut incoming, target, state)?;
                }
                Instruction::Exit => {
                    if state[0] != RegisterKind::Scalar {
                        return Err(SystemError::EINVAL);
                    }
                }
            }
        }

        Ok(Self { instructions })
    }

    /// Execute an already verified, forward-only program. A future unexpected
    /// dispatch failure denies access rather than silently dropping the rule.
    pub fn run(&self, ctx: DeviceAccess) -> bool {
        let mut registers = [0u64; REGISTER_COUNT];
        let mut pc = 0;
        while let Some(instruction) = self.instructions.get(pc) {
            match *instruction {
                Instruction::LoadContext { dst, field, .. } => {
                    registers[dst as usize] = match field {
                        0 => ctx.access_type as u64,
                        1 => ctx.major as u64,
                        2 => ctx.minor as u64,
                        _ => return false,
                    };
                }
                Instruction::Alu {
                    width,
                    op,
                    dst,
                    src,
                } => {
                    let rhs = operand_value(src, &registers, width);
                    let lhs = registers[dst as usize];
                    registers[dst as usize] = alu(width, op, lhs, rhs);
                }
                Instruction::Jump { target } => {
                    pc = target;
                    continue;
                }
                Instruction::JumpIf {
                    width,
                    op,
                    dst,
                    src,
                    target,
                } => {
                    let rhs = operand_value(src, &registers, width);
                    if compare(width, op, registers[dst as usize], rhs) {
                        pc = target;
                        continue;
                    }
                }
                Instruction::Exit => return registers[0] != 0,
            }
            pc += 1;
        }
        false
    }
}

fn require_scalar(operand: Operand, state: &State) -> Result<(), SystemError> {
    if let Operand::Register(register) = operand {
        if state[register as usize] != RegisterKind::Scalar {
            return Err(SystemError::EINVAL);
        }
    }
    Ok(())
}

fn propagate(
    incoming: &mut [Option<State>],
    target: usize,
    state: State,
) -> Result<(), SystemError> {
    let Some(slot) = incoming.get_mut(target) else {
        return Err(SystemError::EINVAL);
    };
    if let Some(previous) = slot {
        for (old, new) in previous.iter_mut().zip(state) {
            if *old != new {
                *old = RegisterKind::Uninitialized;
            }
        }
    } else {
        *slot = Some(state);
    }
    Ok(())
}

fn decode(raw: &[u8], pc: usize, count: usize) -> Result<Instruction, SystemError> {
    let opcode = raw[0];
    let dst = raw[1] & 0x0f;
    let src = raw[1] >> 4;
    let off = i16::from_le_bytes([raw[2], raw[3]]);
    let imm = i32::from_le_bytes([raw[4], raw[5], raw[6], raw[7]]);
    let class = opcode & ebpf::BPF_CLS_MASK;

    if opcode == (ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W) {
        if dst >= 10 || src >= 10 || imm != 0 {
            return Err(SystemError::EINVAL);
        }
        let field = match off {
            0 => 0,
            4 => 1,
            8 => 2,
            _ => return Err(SystemError::EINVAL),
        };
        return Ok(Instruction::LoadContext { dst, src, field });
    }

    if class == ebpf::BPF_ALU || class == ebpf::BPF_ALU64 {
        if dst >= 10 || off != 0 {
            return Err(SystemError::EINVAL);
        }
        let width = if class == ebpf::BPF_ALU {
            Width::Bits32
        } else {
            Width::Bits64
        };
        let op = match opcode & ebpf::BPF_ALU_OP_MASK {
            ebpf::BPF_ADD => AluOp::Add,
            ebpf::BPF_SUB => AluOp::Sub,
            ebpf::BPF_MUL => AluOp::Mul,
            ebpf::BPF_OR => AluOp::Or,
            ebpf::BPF_AND => AluOp::And,
            ebpf::BPF_XOR => AluOp::Xor,
            ebpf::BPF_LSH => AluOp::LeftShift,
            ebpf::BPF_RSH => AluOp::RightShift,
            ebpf::BPF_ARSH => AluOp::ArithmeticRightShift,
            ebpf::BPF_MOV => AluOp::Move,
            ebpf::BPF_NEG if opcode & ebpf::BPF_X == 0 && src == 0 && imm == 0 => AluOp::Negate,
            _ => return Err(SystemError::EINVAL),
        };
        let operand = if opcode & ebpf::BPF_X != 0 {
            if src >= 10 || imm != 0 {
                return Err(SystemError::EINVAL);
            }
            Operand::Register(src)
        } else {
            if src != 0 {
                return Err(SystemError::EINVAL);
            }
            Operand::Immediate(imm)
        };
        if matches!(
            op,
            AluOp::LeftShift | AluOp::RightShift | AluOp::ArithmeticRightShift
        ) && matches!(operand, Operand::Immediate(value) if value < 0 || value >= match width { Width::Bits32 => 32, Width::Bits64 => 64 })
        {
            return Err(SystemError::EINVAL);
        }
        return Ok(Instruction::Alu {
            width,
            op,
            dst,
            src: operand,
        });
    }

    if class == ebpf::BPF_JMP || class == ebpf::BPF_JMP32 {
        let op = opcode & ebpf::BPF_ALU_OP_MASK;
        if opcode == ebpf::EXIT {
            if dst != 0 || src != 0 || off != 0 || imm != 0 {
                return Err(SystemError::EINVAL);
            }
            return Ok(Instruction::Exit);
        }
        if off < 0 {
            return Err(SystemError::EINVAL);
        }
        let target = pc + 1 + off as usize;
        if target >= count {
            return Err(SystemError::EINVAL);
        }
        if opcode == ebpf::JA {
            if dst != 0 || src != 0 || imm != 0 {
                return Err(SystemError::EINVAL);
            }
            return Ok(Instruction::Jump { target });
        }
        if dst >= 10 {
            return Err(SystemError::EINVAL);
        }
        let comparison = match op {
            ebpf::BPF_JEQ => CompareOp::Equal,
            ebpf::BPF_JNE => CompareOp::NotEqual,
            ebpf::BPF_JGT => CompareOp::Greater,
            ebpf::BPF_JGE => CompareOp::GreaterEqual,
            ebpf::BPF_JLT => CompareOp::Less,
            ebpf::BPF_JLE => CompareOp::LessEqual,
            ebpf::BPF_JSGT => CompareOp::SignedGreater,
            ebpf::BPF_JSGE => CompareOp::SignedGreaterEqual,
            ebpf::BPF_JSLT => CompareOp::SignedLess,
            ebpf::BPF_JSLE => CompareOp::SignedLessEqual,
            ebpf::BPF_JSET => CompareOp::Set,
            _ => return Err(SystemError::EINVAL),
        };
        let operand = if opcode & ebpf::BPF_X != 0 {
            if src >= 10 || imm != 0 {
                return Err(SystemError::EINVAL);
            }
            Operand::Register(src)
        } else {
            if src != 0 {
                return Err(SystemError::EINVAL);
            }
            Operand::Immediate(imm)
        };
        return Ok(Instruction::JumpIf {
            width: if class == ebpf::BPF_JMP {
                Width::Bits64
            } else {
                Width::Bits32
            },
            op: comparison,
            dst,
            src: operand,
            target,
        });
    }

    Err(SystemError::EINVAL)
}

fn operand_value(operand: Operand, registers: &[u64; REGISTER_COUNT], width: Width) -> u64 {
    match operand {
        Operand::Immediate(value) => match width {
            Width::Bits32 => value as u32 as u64,
            Width::Bits64 => value as i64 as u64,
        },
        Operand::Register(register) => registers[register as usize],
    }
}

fn alu(width: Width, op: AluOp, lhs: u64, rhs: u64) -> u64 {
    match width {
        Width::Bits32 => {
            let lhs = lhs as u32;
            let rhs = rhs as u32;
            let shift = rhs & 31;
            let value = match op {
                AluOp::Add => lhs.wrapping_add(rhs),
                AluOp::Sub => lhs.wrapping_sub(rhs),
                AluOp::Mul => lhs.wrapping_mul(rhs),
                AluOp::Or => lhs | rhs,
                AluOp::And => lhs & rhs,
                AluOp::Xor => lhs ^ rhs,
                AluOp::LeftShift => lhs.wrapping_shl(shift),
                AluOp::RightShift => lhs.wrapping_shr(shift),
                AluOp::ArithmeticRightShift => ((lhs as i32) >> shift) as u32,
                AluOp::Move => rhs,
                AluOp::Negate => lhs.wrapping_neg(),
            };
            value as u64
        }
        Width::Bits64 => {
            let shift = (rhs & 63) as u32;
            match op {
                AluOp::Add => lhs.wrapping_add(rhs),
                AluOp::Sub => lhs.wrapping_sub(rhs),
                AluOp::Mul => lhs.wrapping_mul(rhs),
                AluOp::Or => lhs | rhs,
                AluOp::And => lhs & rhs,
                AluOp::Xor => lhs ^ rhs,
                AluOp::LeftShift => lhs.wrapping_shl(shift),
                AluOp::RightShift => lhs.wrapping_shr(shift),
                AluOp::ArithmeticRightShift => ((lhs as i64) >> shift) as u64,
                AluOp::Move => rhs,
                AluOp::Negate => lhs.wrapping_neg(),
            }
        }
    }
}

fn compare(width: Width, op: CompareOp, lhs: u64, rhs: u64) -> bool {
    let (lhs, rhs) = match width {
        Width::Bits32 => (lhs as u32 as u64, rhs as u32 as u64),
        Width::Bits64 => (lhs, rhs),
    };
    match op {
        CompareOp::Equal => lhs == rhs,
        CompareOp::NotEqual => lhs != rhs,
        CompareOp::Greater => lhs > rhs,
        CompareOp::GreaterEqual => lhs >= rhs,
        CompareOp::Less => lhs < rhs,
        CompareOp::LessEqual => lhs <= rhs,
        CompareOp::SignedGreater => match width {
            Width::Bits32 => (lhs as i32) > (rhs as i32),
            Width::Bits64 => (lhs as i64) > (rhs as i64),
        },
        CompareOp::SignedGreaterEqual => match width {
            Width::Bits32 => (lhs as i32) >= (rhs as i32),
            Width::Bits64 => (lhs as i64) >= (rhs as i64),
        },
        CompareOp::SignedLess => match width {
            Width::Bits32 => (lhs as i32) < (rhs as i32),
            Width::Bits64 => (lhs as i64) < (rhs as i64),
        },
        CompareOp::SignedLessEqual => match width {
            Width::Bits32 => (lhs as i32) <= (rhs as i32),
            Width::Bits64 => (lhs as i64) <= (rhs as i64),
        },
        CompareOp::Set => lhs & rhs != 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn insn(opcode: u8, dst: u8, src: u8, off: i16, imm: i32) -> [u8; INSN_SIZE] {
        let mut bytes = [opcode, dst | (src << 4), 0, 0, 0, 0, 0, 0];
        bytes[2..4].copy_from_slice(&off.to_le_bytes());
        bytes[4..8].copy_from_slice(&imm.to_le_bytes());
        bytes
    }

    fn program(instructions: &[[u8; INSN_SIZE]]) -> Vec<u8> {
        instructions.iter().flatten().copied().collect()
    }

    #[test]
    fn runc_style_device_rule_uses_r1_as_scalar_after_context_loads() {
        let bytes = program(&[
            insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 2, 1, 0, 0),
            insn(ebpf::AND32_IMM, 2, 0, 0, 0xffff),
            insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 3, 1, 0, 0),
            insn(ebpf::RSH32_IMM, 3, 0, 0, 16),
            insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 4, 1, 4, 0),
            insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 5, 1, 8, 0),
            insn(ebpf::JNE_IMM, 2, 0, 7, 2),
            insn(ebpf::MOV32_REG, 1, 3, 0, 0),
            insn(ebpf::AND32_IMM, 1, 0, 0, 6),
            insn(ebpf::JNE_REG, 1, 3, 4, 0),
            insn(ebpf::JNE_IMM, 4, 0, 3, 1),
            insn(ebpf::JNE_IMM, 5, 0, 2, 3),
            insn(ebpf::MOV32_IMM, 0, 0, 0, 1),
            insn(ebpf::EXIT, 0, 0, 0, 0),
            insn(ebpf::MOV32_IMM, 0, 0, 0, 0),
            insn(ebpf::EXIT, 0, 0, 0, 0),
        ]);
        let filter = DeviceProgram::verify(&bytes).unwrap();
        assert!(filter.run(DeviceAccess {
            access_type: (6 << 16) | 2,
            major: 1,
            minor: 3,
        }));
        assert!(!filter.run(DeviceAccess {
            access_type: (1 << 16) | 2,
            major: 1,
            minor: 3,
        }));
        assert!(!filter.run(DeviceAccess {
            access_type: (6 << 16) | 2,
            major: 1,
            minor: 5,
        }));
    }

    #[test]
    fn rejects_unsafe_memory_and_uninitialized_join() {
        for bad in [
            program(&[
                insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 0, 1, 12, 0),
                insn(ebpf::EXIT, 0, 0, 0, 0),
            ]),
            program(&[
                insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 0, 10, -4, 0),
                insn(ebpf::EXIT, 0, 0, 0, 0),
            ]),
            program(&[
                insn(ebpf::MOV32_IMM, 0, 0, 0, 1),
                insn(ebpf::JNE_IMM, 0, 0, 1, 0),
                insn(ebpf::MOV32_IMM, 2, 0, 0, 1),
                insn(ebpf::MOV32_REG, 0, 2, 0, 0),
                insn(ebpf::EXIT, 0, 0, 0, 0),
            ]),
            program(&[
                insn(ebpf::MOV32_IMM, 1, 0, 0, 1),
                insn(ebpf::BPF_LDX | ebpf::BPF_MEM | ebpf::BPF_W, 0, 1, 0, 0),
                insn(ebpf::EXIT, 0, 0, 0, 0),
            ]),
        ] {
            assert!(DeviceProgram::verify(&bad).is_err());
        }
    }

    #[test]
    fn rejects_loops_calls_and_paths_without_exit() {
        for bad in [
            program(&[
                insn(ebpf::MOV32_IMM, 0, 0, 0, 1),
                insn(ebpf::JA, 0, 0, -1, 0),
                insn(ebpf::EXIT, 0, 0, 0, 0),
            ]),
            program(&[
                insn(ebpf::MOV32_IMM, 0, 0, 0, 1),
                insn(ebpf::CALL, 0, 0, 0, 1),
                insn(ebpf::EXIT, 0, 0, 0, 0),
            ]),
            program(&[insn(ebpf::MOV32_IMM, 0, 0, 0, 1)]),
        ] {
            assert!(DeviceProgram::verify(&bad).is_err());
        }
    }
}
