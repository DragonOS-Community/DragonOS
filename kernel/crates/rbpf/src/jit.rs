// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Derived from uBPF <https://github.com/iovisor/ubpf>
// Copyright 2015 Big Switch Networks, Inc
//      (uBPF: JIT algorithm, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust, MetaBuff addition)

use std::{
    fmt::{Error as FormatterError, Formatter},
    io::{Error, ErrorKind},
    mem,
    ops::{Index, IndexMut},
};

use crate::{ebpf, HashMap};

extern crate libc;

type MachineCode = unsafe fn(*mut u8, usize, *mut u8, usize, usize, usize) -> u64;

const PAGE_SIZE: usize = 4096;
// TODO: check how long the page must be to be sure to support an eBPF program of maximum possible
// length
const NUM_PAGES: usize = 1;

// Special values for target_pc in struct Jump
const TARGET_OFFSET: isize = ebpf::PROG_MAX_INSNS as isize;
const TARGET_PC_EXIT: isize = TARGET_OFFSET + 1;

#[derive(Copy, Clone)]
enum OperandSize {
    S8 = 8,
    S16 = 16,
    S32 = 32,
    S64 = 64,
}

// Registers
const RAX: u8 = 0;
const RCX: u8 = 1;
const RDX: u8 = 2;
const RBX: u8 = 3;
const RSP: u8 = 4;
const RBP: u8 = 5;
const RSI: u8 = 6;
const RDI: u8 = 7;
const R8: u8 = 8;
const R9: u8 = 9;
const R10: u8 = 10;
const R11: u8 = 11;
//const R12: u8 = 12;
const R13: u8 = 13;
const R14: u8 = 14;
const R15: u8 = 15;

const REGISTER_MAP_SIZE: usize = 11;
const REGISTER_MAP: [u8; REGISTER_MAP_SIZE] = [
    RAX, // 0  return value
    RDI, // 1  arg 1
    RSI, // 2  arg 2
    RDX, // 3  arg 3
    R9,  // 4  arg 4
    R8,  // 5  arg 5
    RBX, // 6  callee-saved
    R13, // 7  callee-saved
    R14, // 8  callee-saved
    R15, // 9  callee-saved
    RBP, // 10 stack pointer
         // R10 and R11 are used to compute store a constant pointer to mem and to compute offset for
         // LD_ABS_* and LD_IND_* operations, so they are not mapped to any eBPF register.
];

// Return the x86 register for the given eBPF register
fn map_register(r: u8) -> u8 {
    assert!(r < REGISTER_MAP_SIZE as u8);
    REGISTER_MAP[(r % REGISTER_MAP_SIZE as u8) as usize]
}

macro_rules! emit_bytes {
    ( $mem:ident, $data:tt, $t:ty ) => {{
        let size = mem::size_of::<$t>() as usize;
        assert!($mem.offset + size <= $mem.contents.len());
        unsafe {
            let mut ptr = $mem.contents.as_ptr().add($mem.offset) as *mut $t;
            ptr.write_unaligned($data);
        }
        $mem.offset += size;
    }};
}

#[derive(Debug)]
struct Jump {
    offset_loc: usize,
    target_pc: isize,
}

#[derive(Debug)]
struct JitCompiler {
    pc_locs: Vec<usize>,
    special_targets: HashMap<isize, usize>,
    jumps: Vec<Jump>,
}

impl JitCompiler {
    fn new() -> JitCompiler {
        JitCompiler {
            pc_locs: vec![],
            jumps: vec![],
            special_targets: HashMap::new(),
        }
    }

    fn emit1(&self, mem: &mut JitMemory, data: u8) {
        emit_bytes!(mem, data, u8);
    }

    fn emit2(&self, mem: &mut JitMemory, data: u16) {
        emit_bytes!(mem, data, u16);
    }

    fn emit4(&self, mem: &mut JitMemory, data: u32) {
        emit_bytes!(mem, data, u32);
    }

    fn emit8(&self, mem: &mut JitMemory, data: u64) {
        emit_bytes!(mem, data, u64);
    }

    fn emit_modrm(&self, mem: &mut JitMemory, modrm: u8, r: u8, m: u8) {
        assert_eq!((modrm | 0xc0), 0xc0);
        self.emit1(mem, (modrm & 0xc0) | ((r & 0b111) << 3) | (m & 0b111));
    }

    fn emit_modrm_reg2reg(&self, mem: &mut JitMemory, r: u8, m: u8) {
        self.emit_modrm(mem, 0xc0, r, m);
    }

    fn emit_modrm_and_displacement(&self, mem: &mut JitMemory, r: u8, m: u8, d: i32) {
        if d == 0 && (m & 0b111) != RBP {
            self.emit_modrm(mem, 0x00, r, m);
        } else if (-128..=127).contains(&d) {
            self.emit_modrm(mem, 0x40, r, m);
            self.emit1(mem, d as u8);
        } else {
            self.emit_modrm(mem, 0x80, r, m);
            self.emit4(mem, d as u32);
        }
    }

    fn basix_rex_would_set_bits(&self, w: u8, src: u8, dst: u8) -> bool {
        w != 0 || (src & 0b1000) != 0 || (dst & 0b1000) != 0
    }

    fn emit_rex(&self, mem: &mut JitMemory, w: u8, r: u8, x: u8, b: u8) {
        assert_eq!((w | 1), 1);
        assert_eq!((r | 1), 1);
        assert_eq!((x | 1), 1);
        assert_eq!((b | 1), 1);
        self.emit1(mem, 0x40 | (w << 3) | (r << 2) | (x << 1) | b);
    }

    // Emits a REX prefix with the top bit of src and dst.
    // Skipped if no bits would be set.
    fn emit_basic_rex(&self, mem: &mut JitMemory, w: u8, src: u8, dst: u8) {
        if self.basix_rex_would_set_bits(w, src, dst) {
            let is_masked = |val, mask| match val & mask {
                0 => 0,
                _ => 1,
            };
            self.emit_rex(mem, w, is_masked(src, 8), 0, is_masked(dst, 8));
        }
    }

    fn emit_push(&self, mem: &mut JitMemory, r: u8) {
        self.emit_basic_rex(mem, 0, 0, r);
        self.emit1(mem, 0x50 | (r & 0b111));
    }

    fn emit_pop(&self, mem: &mut JitMemory, r: u8) {
        self.emit_basic_rex(mem, 0, 0, r);
        self.emit1(mem, 0x58 | (r & 0b111));
    }

    // REX prefix and ModRM byte
    // We use the MR encoding when there is a choice
    // 'src' is often used as an opcode extension
    fn emit_alu32(&self, mem: &mut JitMemory, op: u8, src: u8, dst: u8) {
        self.emit_basic_rex(mem, 0, src, dst);
        self.emit1(mem, op);
        self.emit_modrm_reg2reg(mem, src, dst);
    }

    // REX prefix, ModRM byte, and 32-bit immediate
    fn emit_alu32_imm32(&self, mem: &mut JitMemory, op: u8, src: u8, dst: u8, imm: i32) {
        self.emit_alu32(mem, op, src, dst);
        self.emit4(mem, imm as u32);
    }

    // REX prefix, ModRM byte, and 8-bit immediate
    fn emit_alu32_imm8(&self, mem: &mut JitMemory, op: u8, src: u8, dst: u8, imm: i8) {
        self.emit_alu32(mem, op, src, dst);
        self.emit1(mem, imm as u8);
    }

    // REX.W prefix and ModRM byte
    // We use the MR encoding when there is a choice
    // 'src' is often used as an opcode extension
    fn emit_alu64(&self, mem: &mut JitMemory, op: u8, src: u8, dst: u8) {
        self.emit_basic_rex(mem, 1, src, dst);
        self.emit1(mem, op);
        self.emit_modrm_reg2reg(mem, src, dst);
    }

    // REX.W prefix, ModRM byte, and 32-bit immediate
    fn emit_alu64_imm32(&self, mem: &mut JitMemory, op: u8, src: u8, dst: u8, imm: i32) {
        self.emit_alu64(mem, op, src, dst);
        self.emit4(mem, imm as u32);
    }

    // REX.W prefix, ModRM byte, and 8-bit immediate
    fn emit_alu64_imm8(&self, mem: &mut JitMemory, op: u8, src: u8, dst: u8, imm: i8) {
        self.emit_alu64(mem, op, src, dst);
        self.emit1(mem, imm as u8);
    }

    // Register to register mov
    fn emit_mov(&self, mem: &mut JitMemory, src: u8, dst: u8) {
        self.emit_alu64(mem, 0x89, src, dst);
    }

    fn emit_cmp_imm32(&self, mem: &mut JitMemory, dst: u8, imm: i32) {
        self.emit_alu64_imm32(mem, 0x81, 7, dst, imm);
    }

    fn emit_cmp(&self, mem: &mut JitMemory, src: u8, dst: u8) {
        self.emit_alu64(mem, 0x39, src, dst);
    }

    fn emit_cmp32_imm32(&self, mem: &mut JitMemory, dst: u8, imm: i32) {
        self.emit_alu32_imm32(mem, 0x81, 7, dst, imm);
    }

    fn emit_cmp32(&self, mem: &mut JitMemory, src: u8, dst: u8) {
        self.emit_alu32(mem, 0x39, src, dst);
    }

    // Load [src + offset] into dst
    fn emit_load(&self, mem: &mut JitMemory, size: OperandSize, src: u8, dst: u8, offset: i32) {
        let data = match size {
            OperandSize::S64 => 1,
            _ => 0,
        };
        self.emit_basic_rex(mem, data, dst, src);

        match size {
            OperandSize::S8 => {
                // movzx
                self.emit1(mem, 0x0f);
                self.emit1(mem, 0xb6);
            }
            OperandSize::S16 => {
                // movzx
                self.emit1(mem, 0x0f);
                self.emit1(mem, 0xb7);
            }
            OperandSize::S32 | OperandSize::S64 => {
                // mov
                self.emit1(mem, 0x8b);
            }
        }

        self.emit_modrm_and_displacement(mem, dst, src, offset);
    }

    // Load sign-extended immediate into register
    fn emit_load_imm(&self, mem: &mut JitMemory, dst: u8, imm: i64) {
        if imm >= i32::MIN as i64 && imm <= i32::MAX as i64 {
            self.emit_alu64_imm32(mem, 0xc7, 0, dst, imm as i32);
        } else {
            // movabs $imm,dst
            self.emit_basic_rex(mem, 1, 0, dst);
            self.emit1(mem, 0xb8 | (dst & 0b111));
            self.emit8(mem, imm as u64);
        }
    }

    // Store register src to [dst + offset]
    fn emit_store(&self, mem: &mut JitMemory, size: OperandSize, src: u8, dst: u8, offset: i32) {
        match size {
            OperandSize::S16 => self.emit1(mem, 0x66), // 16-bit override
            _ => {}
        };
        let (is_s8, is_u64, rexw) = match size {
            OperandSize::S8 => (true, false, 0),
            OperandSize::S64 => (false, true, 1),
            _ => (false, false, 0),
        };
        if is_u64 || (src & 0b1000) != 0 || (dst & 0b1000) != 0 || is_s8 {
            let is_masked = |val, mask| match val & mask {
                0 => 0,
                _ => 1,
            };
            self.emit_rex(mem, rexw, is_masked(src, 8), 0, is_masked(dst, 8));
        }
        match size {
            OperandSize::S8 => self.emit1(mem, 0x88),
            _ => self.emit1(mem, 0x89),
        };
        self.emit_modrm_and_displacement(mem, src, dst, offset);
    }

    // Store immediate to [dst + offset]
    fn emit_store_imm32(
        &self,
        mem: &mut JitMemory,
        size: OperandSize,
        dst: u8,
        offset: i32,
        imm: i32,
    ) {
        match size {
            OperandSize::S16 => self.emit1(mem, 0x66), // 16-bit override
            _ => {}
        };
        match size {
            OperandSize::S64 => self.emit_basic_rex(mem, 1, 0, dst),
            _ => self.emit_basic_rex(mem, 0, 0, dst),
        };
        match size {
            OperandSize::S8 => self.emit1(mem, 0xc6),
            _ => self.emit1(mem, 0xc7),
        };
        self.emit_modrm_and_displacement(mem, 0, dst, offset);
        match size {
            OperandSize::S8 => self.emit1(mem, imm as u8),
            OperandSize::S16 => self.emit2(mem, imm as u16),
            _ => self.emit4(mem, imm as u32),
        };
    }

    fn emit_direct_jcc(&self, mem: &mut JitMemory, code: u8, offset: u32) {
        self.emit1(mem, 0x0f);
        self.emit1(mem, code);
        emit_bytes!(mem, offset, u32);
    }

    fn emit_call(&self, mem: &mut JitMemory, target: usize) {
        // TODO use direct call when possible
        self.emit_load_imm(mem, RAX, target as i64);
        // callq *%rax
        self.emit1(mem, 0xff);
        self.emit1(mem, 0xd0);
    }

    fn emit_jump_offset(&mut self, mem: &mut JitMemory, target_pc: isize) {
        let jump = Jump {
            offset_loc: mem.offset,
            target_pc,
        };
        self.jumps.push(jump);
        self.emit4(mem, 0);
    }

    fn emit_jcc(&mut self, mem: &mut JitMemory, code: u8, target_pc: isize) {
        self.emit1(mem, 0x0f);
        self.emit1(mem, code);
        self.emit_jump_offset(mem, target_pc);
    }

    fn emit_jmp(&mut self, mem: &mut JitMemory, target_pc: isize) {
        self.emit1(mem, 0xe9);
        self.emit_jump_offset(mem, target_pc);
    }

    fn set_anchor(&mut self, mem: &mut JitMemory, target: isize) {
        self.special_targets.insert(target, mem.offset);
    }

    fn emit_muldivmod(
        &mut self,
        mem: &mut JitMemory,
        pc: u16,
        opc: u8,
        src: u8,
        dst: u8,
        imm: i32,
    ) {
        let mul = (opc & ebpf::BPF_ALU_OP_MASK) == (ebpf::MUL32_IMM & ebpf::BPF_ALU_OP_MASK);
        let div = (opc & ebpf::BPF_ALU_OP_MASK) == (ebpf::DIV32_IMM & ebpf::BPF_ALU_OP_MASK);
        let modrm = (opc & ebpf::BPF_ALU_OP_MASK) == (ebpf::MOD32_IMM & ebpf::BPF_ALU_OP_MASK);
        let is64 = (opc & ebpf::BPF_CLS_MASK) == ebpf::BPF_ALU64;
        let is_reg = (opc & ebpf::BPF_X) == ebpf::BPF_X;

        if (div || mul) && !is_reg && imm == 0 {
            // Division by zero returns 0
            // Set register to 0: xor with itself
            self.emit_alu32(mem, 0x31, dst, dst);
            return;
        }
        if modrm && !is_reg && imm == 0 {
            // Modulo remainder of division by zero keeps destination register unchanged
            return;
        }
        if (div || modrm) && is_reg {
            self.emit_load_imm(mem, RCX, pc as i64);

            // test src,src
            if is64 {
                self.emit_alu64(mem, 0x85, src, src);
            } else {
                self.emit_alu32(mem, 0x85, src, src);
            }

            if div {
                // No division by 0: skip next instructions
                // Jump offset: emit_alu32 adds 2 to 3 bytes, emit_jmp adds 5
                let offset = match self.basix_rex_would_set_bits(0, dst, dst) {
                    true => 3 + 5,
                    false => 2 + 5,
                };
                self.emit_direct_jcc(mem, 0x85, offset);
                // Division by 0: set dst to 0 then go to next instruction
                // Set register to 0: xor with itself
                self.emit_alu32(mem, 0x31, dst, dst);
                self.emit_jmp(mem, (pc + 1) as isize);
            }
            if modrm {
                // Modulo by zero: keep destination register unchanged
                self.emit_jcc(mem, 0x84, (pc + 1) as isize);
            }
        }

        if dst != RAX {
            self.emit_push(mem, RAX);
        }
        if dst != RDX {
            self.emit_push(mem, RDX);
        }
        if imm != 0 {
            self.emit_load_imm(mem, RCX, imm as i64);
        } else {
            self.emit_mov(mem, src, RCX);
        }

        self.emit_mov(mem, dst, RAX);

        if div || modrm {
            // Set register to 0: xor %edx,%edx
            self.emit_alu32(mem, 0x31, RDX, RDX);
        }

        if is64 {
            self.emit_rex(mem, 1, 0, 0, 0);
        }

        // mul %ecx or div %ecx
        self.emit_alu32(mem, 0xf7, if mul { 4 } else { 6 }, RCX);

        if dst != RDX {
            if modrm {
                self.emit_mov(mem, RDX, dst);
            }
            self.emit_pop(mem, RDX);
        }
        if dst != RAX {
            if div || mul {
                self.emit_mov(mem, RAX, dst);
            }
            self.emit_pop(mem, RAX);
        }
    }

    fn jit_compile(
        &mut self,
        mem: &mut JitMemory,
        prog: &[u8],
        use_mbuff: bool,
        update_data_ptr: bool,
        helpers: &HashMap<u32, ebpf::Helper>,
    ) -> Result<(), Error> {
        self.emit_push(mem, RBP);
        self.emit_push(mem, RBX);
        self.emit_push(mem, R13);
        self.emit_push(mem, R14);
        self.emit_push(mem, R15);

        // RDI: mbuff
        // RSI: mbuff_len
        // RDX: mem
        // RCX: mem_len
        // R8:  mem_offset
        // R9:  mem_end_offset

        // Save mem pointer for use with LD_ABS_* and LD_IND_* instructions
        self.emit_mov(mem, RDX, R10);

        match (use_mbuff, update_data_ptr) {
            (false, _) => {
                // We do not use any mbuff. Move mem pointer into register 1.
                if map_register(1) != RDX {
                    self.emit_mov(mem, RDX, map_register(1));
                }
            }
            (true, false) => {
                // We use a mbuff already pointing to mem and mem_end: move it to register 1.
                if map_register(1) != RDI {
                    self.emit_mov(mem, RDI, map_register(1));
                }
            }
            (true, true) => {
                // We have a fixed (simulated) mbuff: update mem and mem_end offset values in it.
                // Store mem at mbuff + mem_offset. Trash R8.
                self.emit_alu64(mem, 0x01, RDI, R8); // add mbuff to mem_offset in R8
                self.emit_store(mem, OperandSize::S64, RDX, R8, 0); // set mem at mbuff + mem_offset
                                                                    // Store mem_end at mbuff + mem_end_offset. Trash R9.
                self.emit_load(mem, OperandSize::S64, RDX, R8, 0); // load mem into R8
                self.emit_alu64(mem, 0x01, RCX, R8); // add mem_len to mem (= mem_end)
                self.emit_alu64(mem, 0x01, RDI, R9); // add mbuff to mem_end_offset
                self.emit_store(mem, OperandSize::S64, R8, R9, 0); // store mem_end

                // Move rdi into register 1
                if map_register(1) != RDI {
                    self.emit_mov(mem, RDI, map_register(1));
                }
            }
        }

        // Copy stack pointer to R10
        self.emit_mov(mem, RSP, map_register(10));

        // Allocate stack space
        self.emit_alu64_imm32(mem, 0x81, 5, RSP, ebpf::STACK_SIZE as i32);

        self.pc_locs = vec![0; prog.len() / ebpf::INSN_SIZE + 1];

        let mut insn_ptr: usize = 0;
        while insn_ptr * ebpf::INSN_SIZE < prog.len() {
            let insn = ebpf::get_insn(prog, insn_ptr);

            self.pc_locs[insn_ptr] = mem.offset;

            let dst = map_register(insn.dst);
            let src = map_register(insn.src);
            let target_pc = insn_ptr as isize + insn.off as isize + 1;

            match insn.opc {
                // BPF_LD class
                // R10 is a constant pointer to mem.
                ebpf::LD_ABS_B => self.emit_load(mem, OperandSize::S8, R10, RAX, insn.imm),
                ebpf::LD_ABS_H => self.emit_load(mem, OperandSize::S16, R10, RAX, insn.imm),
                ebpf::LD_ABS_W => self.emit_load(mem, OperandSize::S32, R10, RAX, insn.imm),
                ebpf::LD_ABS_DW => self.emit_load(mem, OperandSize::S64, R10, RAX, insn.imm),
                ebpf::LD_IND_B => {
                    self.emit_mov(mem, R10, R11); // load mem into R11
                    self.emit_alu64(mem, 0x01, src, R11); // add src to R11
                    self.emit_load(mem, OperandSize::S8, R11, RAX, insn.imm); // ld R0, mem[src+imm]
                }
                ebpf::LD_IND_H => {
                    self.emit_mov(mem, R10, R11); // load mem into R11
                    self.emit_alu64(mem, 0x01, src, R11); // add src to R11
                    self.emit_load(mem, OperandSize::S16, R11, RAX, insn.imm); // ld R0, mem[src+imm]
                }
                ebpf::LD_IND_W => {
                    self.emit_mov(mem, R10, R11); // load mem into R11
                    self.emit_alu64(mem, 0x01, src, R11); // add src to R11
                    self.emit_load(mem, OperandSize::S32, R11, RAX, insn.imm); // ld R0, mem[src+imm]
                }
                ebpf::LD_IND_DW => {
                    self.emit_mov(mem, R10, R11); // load mem into R11
                    self.emit_alu64(mem, 0x01, src, R11); // add src to R11
                    self.emit_load(mem, OperandSize::S64, R11, RAX, insn.imm); // ld R0, mem[src+imm]
                }

                ebpf::LD_DW_IMM => {
                    insn_ptr += 1;
                    let second_part = ebpf::get_insn(prog, insn_ptr).imm as u64;
                    let imm = (insn.imm as u32) as u64 | second_part.wrapping_shl(32);
                    self.emit_load_imm(mem, dst, imm as i64);
                }

                // BPF_LDX class
                ebpf::LD_B_REG => self.emit_load(mem, OperandSize::S8, src, dst, insn.off as i32),
                ebpf::LD_H_REG => self.emit_load(mem, OperandSize::S16, src, dst, insn.off as i32),
                ebpf::LD_W_REG => self.emit_load(mem, OperandSize::S32, src, dst, insn.off as i32),
                ebpf::LD_DW_REG => self.emit_load(mem, OperandSize::S64, src, dst, insn.off as i32),

                // BPF_ST class
                ebpf::ST_B_IMM => {
                    self.emit_store_imm32(mem, OperandSize::S8, dst, insn.off as i32, insn.imm)
                }
                ebpf::ST_H_IMM => {
                    self.emit_store_imm32(mem, OperandSize::S16, dst, insn.off as i32, insn.imm)
                }
                ebpf::ST_W_IMM => {
                    self.emit_store_imm32(mem, OperandSize::S32, dst, insn.off as i32, insn.imm)
                }
                ebpf::ST_DW_IMM => {
                    self.emit_store_imm32(mem, OperandSize::S64, dst, insn.off as i32, insn.imm)
                }

                // BPF_STX class
                ebpf::ST_B_REG => self.emit_store(mem, OperandSize::S8, src, dst, insn.off as i32),
                ebpf::ST_H_REG => self.emit_store(mem, OperandSize::S16, src, dst, insn.off as i32),
                ebpf::ST_W_REG => self.emit_store(mem, OperandSize::S32, src, dst, insn.off as i32),
                ebpf::ST_DW_REG => {
                    self.emit_store(mem, OperandSize::S64, src, dst, insn.off as i32)
                }
                ebpf::ST_W_XADD => unimplemented!(),
                ebpf::ST_DW_XADD => unimplemented!(),

                // BPF_ALU class
                ebpf::ADD32_IMM => self.emit_alu32_imm32(mem, 0x81, 0, dst, insn.imm),
                ebpf::ADD32_REG => self.emit_alu32(mem, 0x01, src, dst),
                ebpf::SUB32_IMM => self.emit_alu32_imm32(mem, 0x81, 5, dst, insn.imm),
                ebpf::SUB32_REG => self.emit_alu32(mem, 0x29, src, dst),
                ebpf::MUL32_IMM
                | ebpf::MUL32_REG
                | ebpf::DIV32_IMM
                | ebpf::DIV32_REG
                | ebpf::MOD32_IMM
                | ebpf::MOD32_REG => {
                    self.emit_muldivmod(mem, insn_ptr as u16, insn.opc, src, dst, insn.imm)
                }
                ebpf::OR32_IMM => self.emit_alu32_imm32(mem, 0x81, 1, dst, insn.imm),
                ebpf::OR32_REG => self.emit_alu32(mem, 0x09, src, dst),
                ebpf::AND32_IMM => self.emit_alu32_imm32(mem, 0x81, 4, dst, insn.imm),
                ebpf::AND32_REG => self.emit_alu32(mem, 0x21, src, dst),
                ebpf::LSH32_IMM => self.emit_alu32_imm8(mem, 0xc1, 4, dst, insn.imm as i8),
                ebpf::LSH32_REG => {
                    self.emit_mov(mem, src, RCX);
                    self.emit_alu32(mem, 0xd3, 4, dst);
                }
                ebpf::RSH32_IMM => self.emit_alu32_imm8(mem, 0xc1, 5, dst, insn.imm as i8),
                ebpf::RSH32_REG => {
                    self.emit_mov(mem, src, RCX);
                    self.emit_alu32(mem, 0xd3, 5, dst);
                }
                ebpf::NEG32 => self.emit_alu32(mem, 0xf7, 3, dst),
                ebpf::XOR32_IMM => self.emit_alu32_imm32(mem, 0x81, 6, dst, insn.imm),
                ebpf::XOR32_REG => self.emit_alu32(mem, 0x31, src, dst),
                ebpf::MOV32_IMM => self.emit_alu32_imm32(mem, 0xc7, 0, dst, insn.imm),
                ebpf::MOV32_REG => self.emit_mov(mem, src, dst),
                ebpf::ARSH32_IMM => self.emit_alu32_imm8(mem, 0xc1, 7, dst, insn.imm as i8),
                ebpf::ARSH32_REG => {
                    self.emit_mov(mem, src, RCX);
                    self.emit_alu32(mem, 0xd3, 7, dst);
                }
                ebpf::LE => {} // No-op
                ebpf::BE => {
                    match insn.imm {
                        16 => {
                            // rol
                            self.emit1(mem, 0x66); // 16-bit override
                            self.emit_alu32_imm8(mem, 0xc1, 0, dst, 8);
                            // and
                            self.emit_alu32_imm32(mem, 0x81, 4, dst, 0xffff);
                        }
                        32 | 64 => {
                            // bswap
                            let bit = match insn.imm {
                                64 => 1,
                                _ => 0,
                            };
                            self.emit_basic_rex(mem, bit, 0, dst);
                            self.emit1(mem, 0x0f);
                            self.emit1(mem, 0xc8 | (dst & 0b111));
                        }
                        _ => unreachable!(), // Should have been caught by verifier
                    }
                }

                // BPF_ALU64 class
                ebpf::ADD64_IMM => self.emit_alu64_imm32(mem, 0x81, 0, dst, insn.imm),
                ebpf::ADD64_REG => self.emit_alu64(mem, 0x01, src, dst),
                ebpf::SUB64_IMM => self.emit_alu64_imm32(mem, 0x81, 5, dst, insn.imm),
                ebpf::SUB64_REG => self.emit_alu64(mem, 0x29, src, dst),
                ebpf::MUL64_IMM
                | ebpf::MUL64_REG
                | ebpf::DIV64_IMM
                | ebpf::DIV64_REG
                | ebpf::MOD64_IMM
                | ebpf::MOD64_REG => {
                    self.emit_muldivmod(mem, insn_ptr as u16, insn.opc, src, dst, insn.imm)
                }
                ebpf::OR64_IMM => self.emit_alu64_imm32(mem, 0x81, 1, dst, insn.imm),
                ebpf::OR64_REG => self.emit_alu64(mem, 0x09, src, dst),
                ebpf::AND64_IMM => self.emit_alu64_imm32(mem, 0x81, 4, dst, insn.imm),
                ebpf::AND64_REG => self.emit_alu64(mem, 0x21, src, dst),
                ebpf::LSH64_IMM => self.emit_alu64_imm8(mem, 0xc1, 4, dst, insn.imm as i8),
                ebpf::LSH64_REG => {
                    self.emit_mov(mem, src, RCX);
                    self.emit_alu64(mem, 0xd3, 4, dst);
                }
                ebpf::RSH64_IMM => self.emit_alu64_imm8(mem, 0xc1, 5, dst, insn.imm as i8),
                ebpf::RSH64_REG => {
                    self.emit_mov(mem, src, RCX);
                    self.emit_alu64(mem, 0xd3, 5, dst);
                }
                ebpf::NEG64 => self.emit_alu64(mem, 0xf7, 3, dst),
                ebpf::XOR64_IMM => self.emit_alu64_imm32(mem, 0x81, 6, dst, insn.imm),
                ebpf::XOR64_REG => self.emit_alu64(mem, 0x31, src, dst),
                ebpf::MOV64_IMM => self.emit_load_imm(mem, dst, insn.imm as i64),
                ebpf::MOV64_REG => self.emit_mov(mem, src, dst),
                ebpf::ARSH64_IMM => self.emit_alu64_imm8(mem, 0xc1, 7, dst, insn.imm as i8),
                ebpf::ARSH64_REG => {
                    self.emit_mov(mem, src, RCX);
                    self.emit_alu64(mem, 0xd3, 7, dst);
                }

                // BPF_JMP class
                ebpf::JA => self.emit_jmp(mem, target_pc),
                ebpf::JEQ_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x84, target_pc);
                }
                ebpf::JEQ_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x84, target_pc);
                }
                ebpf::JGT_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x87, target_pc);
                }
                ebpf::JGT_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x87, target_pc);
                }
                ebpf::JGE_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x83, target_pc);
                }
                ebpf::JGE_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x83, target_pc);
                }
                ebpf::JLT_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x82, target_pc);
                }
                ebpf::JLT_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x82, target_pc);
                }
                ebpf::JLE_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x86, target_pc);
                }
                ebpf::JLE_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x86, target_pc);
                }
                ebpf::JSET_IMM => {
                    self.emit_alu64_imm32(mem, 0xf7, 0, dst, insn.imm);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JSET_REG => {
                    self.emit_alu64(mem, 0x85, src, dst);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JNE_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JNE_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JSGT_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8f, target_pc);
                }
                ebpf::JSGT_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x8f, target_pc);
                }
                ebpf::JSGE_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8d, target_pc);
                }
                ebpf::JSGE_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x8d, target_pc);
                }
                ebpf::JSLT_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8c, target_pc);
                }
                ebpf::JSLT_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x8c, target_pc);
                }
                ebpf::JSLE_IMM => {
                    self.emit_cmp_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8e, target_pc);
                }
                ebpf::JSLE_REG => {
                    self.emit_cmp(mem, src, dst);
                    self.emit_jcc(mem, 0x8e, target_pc);
                }

                // BPF_JMP32 class
                ebpf::JEQ_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x84, target_pc);
                }
                ebpf::JEQ_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x84, target_pc);
                }
                ebpf::JGT_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x87, target_pc);
                }
                ebpf::JGT_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x87, target_pc);
                }
                ebpf::JGE_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x83, target_pc);
                }
                ebpf::JGE_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x83, target_pc);
                }
                ebpf::JLT_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x82, target_pc);
                }
                ebpf::JLT_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x82, target_pc);
                }
                ebpf::JLE_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x86, target_pc);
                }
                ebpf::JLE_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x86, target_pc);
                }
                ebpf::JSET_IMM32 => {
                    self.emit_alu32_imm32(mem, 0xf7, 0, dst, insn.imm);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JSET_REG32 => {
                    self.emit_alu32(mem, 0x85, src, dst);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JNE_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JNE_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x85, target_pc);
                }
                ebpf::JSGT_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8f, target_pc);
                }
                ebpf::JSGT_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x8f, target_pc);
                }
                ebpf::JSGE_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8d, target_pc);
                }
                ebpf::JSGE_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x8d, target_pc);
                }
                ebpf::JSLT_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8c, target_pc);
                }
                ebpf::JSLT_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x8c, target_pc);
                }
                ebpf::JSLE_IMM32 => {
                    self.emit_cmp32_imm32(mem, dst, insn.imm);
                    self.emit_jcc(mem, 0x8e, target_pc);
                }
                ebpf::JSLE_REG32 => {
                    self.emit_cmp32(mem, src, dst);
                    self.emit_jcc(mem, 0x8e, target_pc);
                }

                ebpf::CALL => {
                    // For JIT, helpers in use MUST be registered at compile time. They can be
                    // updated later, but not created after compiling (we need the address of the
                    // helper function in the JIT-compiled program).
                    if let Some(helper) = helpers.get(&(insn.imm as u32)) {
                        // We reserve RCX for shifts
                        self.emit_mov(mem, R9, RCX);
                        self.emit_call(mem, *helper as usize);
                    } else {
                        Err(Error::new(
                            ErrorKind::Other,
                            format!(
                                "[JIT] Error: unknown helper function (id: {:#x})",
                                insn.imm as u32
                            ),
                        ))?;
                    };
                }
                ebpf::TAIL_CALL => {
                    unimplemented!()
                }
                ebpf::EXIT => {
                    if insn_ptr != prog.len() / ebpf::INSN_SIZE - 1 {
                        self.emit_jmp(mem, TARGET_PC_EXIT);
                    };
                }

                _ => {
                    Err(Error::new(
                        ErrorKind::Other,
                        format!(
                            "[JIT] Error: unknown eBPF opcode {:#2x} (insn #{insn_ptr:?})",
                            insn.opc
                        ),
                    ))?;
                }
            }

            insn_ptr += 1;
        }

        // Epilogue
        self.set_anchor(mem, TARGET_PC_EXIT);

        // Move register 0 into rax
        if map_register(0) != RAX {
            self.emit_mov(mem, map_register(0), RAX);
        }

        // Deallocate stack space
        self.emit_alu64_imm32(mem, 0x81, 0, RSP, ebpf::STACK_SIZE as i32);

        self.emit_pop(mem, R15);
        self.emit_pop(mem, R14);
        self.emit_pop(mem, R13);
        self.emit_pop(mem, RBX);
        self.emit_pop(mem, RBP);

        self.emit1(mem, 0xc3); // ret

        Ok(())
    }

    fn resolve_jumps(&mut self, mem: &mut JitMemory) -> Result<(), Error> {
        for jump in &self.jumps {
            let target_loc = match self.special_targets.get(&jump.target_pc) {
                Some(target) => *target,
                None => self.pc_locs[jump.target_pc as usize],
            };

            // Assumes jump offset is at end of instruction
            unsafe {
                let offset_loc = jump.offset_loc as i32 + std::mem::size_of::<i32>() as i32;
                let rel = &(target_loc as i32 - offset_loc) as *const i32;

                let offset_ptr = mem.contents.as_ptr().add(jump.offset_loc);

                libc::memcpy(
                    offset_ptr as *mut libc::c_void,
                    rel as *const libc::c_void,
                    std::mem::size_of::<i32>(),
                );
            }
        }
        Ok(())
    }
} // impl JitCompiler

pub struct JitMemory<'a> {
    contents: &'a mut [u8],
    offset: usize,
}

impl<'a> JitMemory<'a> {
    pub fn new(
        prog: &[u8],
        helpers: &HashMap<u32, ebpf::Helper>,
        use_mbuff: bool,
        update_data_ptr: bool,
    ) -> Result<JitMemory<'a>, Error> {
        let contents: &mut [u8];
        let mut raw: mem::MaybeUninit<*mut libc::c_void> = mem::MaybeUninit::uninit();
        unsafe {
            let size = NUM_PAGES * PAGE_SIZE;
            libc::posix_memalign(raw.as_mut_ptr(), PAGE_SIZE, size);
            libc::mprotect(
                *raw.as_mut_ptr(),
                size,
                libc::PROT_EXEC | libc::PROT_READ | libc::PROT_WRITE,
            );
            std::ptr::write_bytes(*raw.as_mut_ptr(), 0xc3, size); // for now, prepopulate with 'RET' calls
            contents =
                std::slice::from_raw_parts_mut(*raw.as_mut_ptr() as *mut u8, NUM_PAGES * PAGE_SIZE);
            raw.assume_init();
        }

        let mut mem = JitMemory {
            contents,
            offset: 0,
        };

        let mut jit = JitCompiler::new();
        jit.jit_compile(&mut mem, prog, use_mbuff, update_data_ptr, helpers)?;
        jit.resolve_jumps(&mut mem)?;

        Ok(mem)
    }

    pub fn get_prog(&self) -> MachineCode {
        unsafe { mem::transmute(self.contents.as_ptr()) }
    }
}

impl<'a> Index<usize> for JitMemory<'a> {
    type Output = u8;

    fn index(&self, _index: usize) -> &u8 {
        &self.contents[_index]
    }
}

impl<'a> IndexMut<usize> for JitMemory<'a> {
    fn index_mut(&mut self, _index: usize) -> &mut u8 {
        &mut self.contents[_index]
    }
}

impl<'a> Drop for JitMemory<'a> {
    fn drop(&mut self) {
        unsafe {
            libc::free(self.contents.as_mut_ptr() as *mut libc::c_void);
        }
    }
}

impl<'a> std::fmt::Debug for JitMemory<'a> {
    fn fmt(&self, fmt: &mut Formatter) -> Result<(), FormatterError> {
        fmt.write_str("JIT contents: [")?;
        fmt.write_str(" ] | ")?;
        fmt.debug_struct("JIT memory")
            .field("offset", &self.offset)
            .finish()
    }
}
