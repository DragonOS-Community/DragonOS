// SPDX-License-Identifier: (Apache-2.0 OR MIT)
// Derived from uBPF <https://github.com/iovisor/ubpf>
// Copyright 2015 Big Switch Networks, Inc
//      (uBPF: VM architecture, parts of the interpreter, originally in C)
// Copyright 2016 6WIND S.A. <quentin.monnet@6wind.com>
//      (Translation to Rust, MetaBuff/multiple classes addition, hashmaps for helpers)

use crate::{
    ebpf::{self, Insn},
    helpers::BPF_FUNC_MAPPER,
    stack::StackFrame,
    *,
};

#[cfg(not(feature = "user"))]
#[allow(unused)]
fn check_mem(
    addr: u64,
    len: usize,
    access_type: &str,
    insn_ptr: usize,
    mbuff: &[u8],
    mem: &[u8],
    stack: &[u8],
) -> Result<(), Error> {
    log::trace!(
        "check_mem: addr {:#x}, len {}, access_type {}, insn_ptr {}",
        addr,
        len,
        access_type,
        insn_ptr
    );
    log::trace!(
        "check_mem: mbuff: {:#x}/{:#x}, mem: {:#x}/{:#x}, stack: {:#x}/{:#x}",
        mbuff.as_ptr() as u64,
        mbuff.len(),
        mem.as_ptr() as u64,
        mem.len(),
        stack.as_ptr() as u64,
        stack.len()
    );
    Ok(())
}

#[cfg(feature = "user")]
fn check_mem(
    addr: u64,
    len: usize,
    access_type: &str,
    insn_ptr: usize,
    mbuff: &[u8],
    mem: &[u8],
    stack: &[u8],
) -> Result<(), Error> {
    if let Some(addr_end) = addr.checked_add(len as u64) {
        if mbuff.as_ptr() as u64 <= addr && addr_end <= mbuff.as_ptr() as u64 + mbuff.len() as u64 {
            return Ok(());
        }
        if mem.as_ptr() as u64 <= addr && addr_end <= mem.as_ptr() as u64 + mem.len() as u64 {
            return Ok(());
        }
        if stack.as_ptr() as u64 <= addr && addr_end <= stack.as_ptr() as u64 + stack.len() as u64 {
            return Ok(());
        }
    }

    Err(Error::new(ErrorKind::Other, format!(
        "Error: out of bounds memory {} (insn #{:?}), addr {:#x}, size {:?}\nmbuff: {:#x}/{:#x}, mem: {:#x}/{:#x}, stack: {:#x}/{:#x}",
        access_type, insn_ptr, addr, len,
        mbuff.as_ptr() as u64, mbuff.len(),
        mem.as_ptr() as u64, mem.len(),
        stack.as_ptr() as u64, stack.len()
    )))
}

#[inline]
fn do_jump(insn_ptr: &mut usize, insn: &Insn) {
    *insn_ptr = (*insn_ptr as i16 + insn.off) as usize;
}

#[allow(unknown_lints)]
#[allow(cyclomatic_complexity)]
pub fn execute_program(
    prog_: Option<&[u8]>,
    mem: &[u8],
    mbuff: &[u8],
    helpers: &HashMap<u32, ebpf::Helper>,
) -> Result<u64, Error> {
    const U32MAX: u64 = u32::MAX as u64;
    const SHIFT_MASK_64: u64 = 0x3f;

    let prog = match prog_ {
        Some(prog) => prog,
        None => Err(Error::new(
            ErrorKind::Other,
            "Error: No program set, call prog_set() to load one",
        ))?,
    };
    let mut stacks = Vec::new();
    let stack = StackFrame::new();
    // R1 points to beginning of memory area, R10 to stack
    let mut reg: [u64; 11] = [
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        stack.as_ptr() as u64 + stack.len() as u64,
    ];
    stacks.push(stack);
    if !mbuff.is_empty() {
        reg[1] = mbuff.as_ptr() as u64;
    } else if !mem.is_empty() {
        reg[1] = mem.as_ptr() as u64;
    }
    let check_mem_load =
        |stack: &[u8], addr: u64, len: usize, insn_ptr: usize| -> Result<(), Error> {
            check_mem(addr, len, "load", insn_ptr, mbuff, mem, stack)
        };
    let check_mem_store =
        |stack: &[u8], addr: u64, len: usize, insn_ptr: usize| -> Result<(), Error> {
            check_mem(addr, len, "store", insn_ptr, mbuff, mem, stack)
        };

    // Loop on instructions
    let mut insn_ptr: usize = 0;
    while insn_ptr * ebpf::INSN_SIZE < prog.len() {
        let insn = ebpf::get_insn(prog, insn_ptr);
        insn_ptr += 1;
        let _dst = insn.dst as usize;
        let _src = insn.src as usize;

        match insn.opc {
            // BPF_LD class
            // LD_ABS_* and LD_IND_* are supposed to load pointer to data from metadata buffer.
            // Since this pointer is constant, and since we already know it (mem), do not
            // bother re-fetching it, just use mem already.
            ebpf::LD_ABS_B => {
                reg[0] = unsafe {
                    let x = (mem.as_ptr() as u64 + (insn.imm as u32) as u64) as *const u8;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_ABS_H => {
                reg[0] = unsafe {
                    let x = (mem.as_ptr() as u64 + (insn.imm as u32) as u64) as *const u16;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_ABS_W => {
                reg[0] = unsafe {
                    let x = (mem.as_ptr() as u64 + (insn.imm as u32) as u64) as *const u32;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_ABS_DW => {
                log::info!("executing LD_ABS_DW, set reg[{}] to {:#x}", _dst, insn.imm);
                reg[0] = unsafe {
                    let x = (mem.as_ptr() as u64 + (insn.imm as u32) as u64) as *const u64;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned()
                }
            }
            ebpf::LD_IND_B => {
                reg[0] = unsafe {
                    let x =
                        (mem.as_ptr() as u64 + reg[_src] + (insn.imm as u32) as u64) as *const u8;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_IND_H => {
                reg[0] = unsafe {
                    let x =
                        (mem.as_ptr() as u64 + reg[_src] + (insn.imm as u32) as u64) as *const u16;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_IND_W => {
                reg[0] = unsafe {
                    let x =
                        (mem.as_ptr() as u64 + reg[_src] + (insn.imm as u32) as u64) as *const u32;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_IND_DW => {
                reg[0] = unsafe {
                    let x =
                        (mem.as_ptr() as u64 + reg[_src] + (insn.imm as u32) as u64) as *const u64;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned()
                }
            }

            ebpf::LD_DW_IMM => {
                let next_insn = ebpf::get_insn(prog, insn_ptr);
                insn_ptr += 1;
                // log::warn!(
                //     "executing LD_DW_IMM, set reg[{}] to {:#x}",
                //     _dst,
                //     ((insn.imm as u32) as u64) + ((next_insn.imm as u64) << 32)
                // );
                reg[_dst] = ((insn.imm as u32) as u64) + ((next_insn.imm as u64) << 32);
            }

            // BPF_LDX class
            ebpf::LD_B_REG => {
                reg[_dst] = unsafe {
                    #[allow(clippy::cast_ptr_alignment)]
                    let x = (reg[_src] as *const u8).offset(insn.off as isize);
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 1, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_H_REG => {
                reg[_dst] = unsafe {
                    #[allow(clippy::cast_ptr_alignment)]
                    let x = (reg[_src] as *const u8).offset(insn.off as isize) as *const u16;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 2, insn_ptr)?;
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_W_REG => {
                reg[_dst] = unsafe {
                    #[allow(clippy::cast_ptr_alignment)]
                    let x = (reg[_src] as *const u8).offset(insn.off as isize) as *const u32;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 4, insn_ptr)?;
                    // log::warn!(
                    //     "executing LD_W_REG, the ptr is REG:{} -> [{:#x}] + {:#x}",
                    //     _src,
                    //     reg[_src],
                    //     insn.off
                    // );
                    x.read_unaligned() as u64
                }
            }
            ebpf::LD_DW_REG => {
                reg[_dst] = unsafe {
                    #[allow(clippy::cast_ptr_alignment)]
                    let x = (reg[_src] as *const u8).offset(insn.off as isize) as *const u64;
                    check_mem_load(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                    x.read_unaligned()
                }
            }

            // BPF_ST class
            ebpf::ST_B_IMM => unsafe {
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u8;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 1, insn_ptr)?;
                x.write_unaligned(insn.imm as u8);
            },
            ebpf::ST_H_IMM => unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u16;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 2, insn_ptr)?;
                x.write_unaligned(insn.imm as u16);
            },
            ebpf::ST_W_IMM => unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u32;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 4, insn_ptr)?;
                x.write_unaligned(insn.imm as u32);
            },
            ebpf::ST_DW_IMM => unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u64;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                x.write_unaligned(insn.imm as u64);
            },

            // BPF_STX class
            ebpf::ST_B_REG => unsafe {
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u8;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 1, insn_ptr)?;
                x.write_unaligned(reg[_src] as u8);
            },
            ebpf::ST_H_REG => unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u16;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 2, insn_ptr)?;
                x.write_unaligned(reg[_src] as u16);
            },
            ebpf::ST_W_REG => unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u32;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 4, insn_ptr)?;
                x.write_unaligned(reg[_src] as u32);
            },
            ebpf::ST_DW_REG => unsafe {
                #[allow(clippy::cast_ptr_alignment)]
                let x = (reg[_dst] as *const u8).offset(insn.off as isize) as *mut u64;
                check_mem_store(stacks.last().unwrap().as_slice(), x as u64, 8, insn_ptr)?;
                x.write_unaligned(reg[_src]);
            },
            ebpf::ST_W_XADD => unimplemented!(),
            ebpf::ST_DW_XADD => unimplemented!(),

            // BPF_ALU class
            // TODO Check how overflow works in kernel. Should we &= U32MAX all src register value
            // before we do the operation?
            // Cf ((0x11 << 32) - (0x1 << 32)) as u32 VS ((0x11 << 32) as u32 - (0x1 << 32) as u32
            ebpf::ADD32_IMM => reg[_dst] = (reg[_dst] as i32).wrapping_add(insn.imm) as u64, //((reg[_dst] & U32MAX) + insn.imm  as u64)     & U32MAX,
            ebpf::ADD32_REG => reg[_dst] = (reg[_dst] as i32).wrapping_add(reg[_src] as i32) as u64, //((reg[_dst] & U32MAX) + (reg[_src] & U32MAX)) & U32MAX,
            ebpf::SUB32_IMM => reg[_dst] = (reg[_dst] as i32).wrapping_sub(insn.imm) as u64,
            ebpf::SUB32_REG => reg[_dst] = (reg[_dst] as i32).wrapping_sub(reg[_src] as i32) as u64,
            ebpf::MUL32_IMM => reg[_dst] = (reg[_dst] as i32).wrapping_mul(insn.imm) as u64,
            ebpf::MUL32_REG => reg[_dst] = (reg[_dst] as i32).wrapping_mul(reg[_src] as i32) as u64,
            ebpf::DIV32_IMM if insn.imm as u32 == 0 => reg[_dst] = 0,
            ebpf::DIV32_IMM => reg[_dst] = (reg[_dst] as u32 / insn.imm as u32) as u64,
            ebpf::DIV32_REG if reg[_src] as u32 == 0 => reg[_dst] = 0,
            ebpf::DIV32_REG => reg[_dst] = (reg[_dst] as u32 / reg[_src] as u32) as u64,
            ebpf::OR32_IMM => reg[_dst] = (reg[_dst] as u32 | insn.imm as u32) as u64,
            ebpf::OR32_REG => reg[_dst] = (reg[_dst] as u32 | reg[_src] as u32) as u64,
            ebpf::AND32_IMM => reg[_dst] = (reg[_dst] as u32 & insn.imm as u32) as u64,
            ebpf::AND32_REG => reg[_dst] = (reg[_dst] as u32 & reg[_src] as u32) as u64,
            // As for the 64-bit version, we should mask the number of bits to shift with
            // 0x1f, but .wrappping_shr() already takes care of it for us.
            ebpf::LSH32_IMM => reg[_dst] = (reg[_dst] as u32).wrapping_shl(insn.imm as u32) as u64,
            ebpf::LSH32_REG => reg[_dst] = (reg[_dst] as u32).wrapping_shl(reg[_src] as u32) as u64,
            ebpf::RSH32_IMM => reg[_dst] = (reg[_dst] as u32).wrapping_shr(insn.imm as u32) as u64,
            ebpf::RSH32_REG => reg[_dst] = (reg[_dst] as u32).wrapping_shr(reg[_src] as u32) as u64,
            ebpf::NEG32 => {
                reg[_dst] = (reg[_dst] as i32).wrapping_neg() as u64;
                reg[_dst] &= U32MAX;
            }
            ebpf::MOD32_IMM if insn.imm as u32 == 0 => (),
            ebpf::MOD32_IMM => reg[_dst] = (reg[_dst] as u32 % insn.imm as u32) as u64,
            ebpf::MOD32_REG if reg[_src] as u32 == 0 => (),
            ebpf::MOD32_REG => reg[_dst] = (reg[_dst] as u32 % reg[_src] as u32) as u64,
            ebpf::XOR32_IMM => reg[_dst] = (reg[_dst] as u32 ^ insn.imm as u32) as u64,
            ebpf::XOR32_REG => reg[_dst] = (reg[_dst] as u32 ^ reg[_src] as u32) as u64,
            ebpf::MOV32_IMM => reg[_dst] = insn.imm as u32 as u64,
            ebpf::MOV32_REG => reg[_dst] = (reg[_src] as u32) as u64,
            // As for the 64-bit version, we should mask the number of bits to shift with
            // 0x1f, but .wrappping_shr() already takes care of it for us.
            ebpf::ARSH32_IMM => {
                reg[_dst] = (reg[_dst] as i32).wrapping_shr(insn.imm as u32) as u64;
                reg[_dst] &= U32MAX;
            }
            ebpf::ARSH32_REG => {
                reg[_dst] = (reg[_dst] as i32).wrapping_shr(reg[_src] as u32) as u64;
                reg[_dst] &= U32MAX;
            }
            ebpf::LE => {
                reg[_dst] = match insn.imm {
                    16 => (reg[_dst] as u16).to_le() as u64,
                    32 => (reg[_dst] as u32).to_le() as u64,
                    64 => reg[_dst].to_le(),
                    _ => unreachable!(),
                };
            }
            ebpf::BE => {
                reg[_dst] = match insn.imm {
                    16 => (reg[_dst] as u16).to_be() as u64,
                    32 => (reg[_dst] as u32).to_be() as u64,
                    64 => reg[_dst].to_be(),
                    _ => unreachable!(),
                };
            }

            // BPF_ALU64 class
            ebpf::ADD64_IMM => reg[_dst] = reg[_dst].wrapping_add(insn.imm as u64),
            ebpf::ADD64_REG => reg[_dst] = reg[_dst].wrapping_add(reg[_src]),
            ebpf::SUB64_IMM => reg[_dst] = reg[_dst].wrapping_sub(insn.imm as u64),
            ebpf::SUB64_REG => reg[_dst] = reg[_dst].wrapping_sub(reg[_src]),
            ebpf::MUL64_IMM => reg[_dst] = reg[_dst].wrapping_mul(insn.imm as u64),
            ebpf::MUL64_REG => reg[_dst] = reg[_dst].wrapping_mul(reg[_src]),
            ebpf::DIV64_IMM if insn.imm == 0 => reg[_dst] = 0,
            ebpf::DIV64_IMM => reg[_dst] /= insn.imm as u64,
            ebpf::DIV64_REG if reg[_src] == 0 => reg[_dst] = 0,
            ebpf::DIV64_REG => reg[_dst] /= reg[_src],
            ebpf::OR64_IMM => reg[_dst] |= insn.imm as u64,
            ebpf::OR64_REG => reg[_dst] |= reg[_src],
            ebpf::AND64_IMM => reg[_dst] &= insn.imm as u64,
            ebpf::AND64_REG => reg[_dst] &= reg[_src],
            ebpf::LSH64_IMM => reg[_dst] <<= insn.imm as u64 & SHIFT_MASK_64,
            ebpf::LSH64_REG => reg[_dst] <<= reg[_src] & SHIFT_MASK_64,
            ebpf::RSH64_IMM => reg[_dst] >>= insn.imm as u64 & SHIFT_MASK_64,
            ebpf::RSH64_REG => reg[_dst] >>= reg[_src] & SHIFT_MASK_64,
            ebpf::NEG64 => reg[_dst] = -(reg[_dst] as i64) as u64,
            ebpf::MOD64_IMM if insn.imm == 0 => (),
            ebpf::MOD64_IMM => reg[_dst] %= insn.imm as u64,
            ebpf::MOD64_REG if reg[_src] == 0 => (),
            ebpf::MOD64_REG => reg[_dst] %= reg[_src],
            ebpf::XOR64_IMM => reg[_dst] ^= insn.imm as u64,
            ebpf::XOR64_REG => reg[_dst] ^= reg[_src],
            ebpf::MOV64_IMM => reg[_dst] = insn.imm as u64,
            ebpf::MOV64_REG => reg[_dst] = reg[_src],
            ebpf::ARSH64_IMM => {
                reg[_dst] = (reg[_dst] as i64 >> (insn.imm as u64 & SHIFT_MASK_64)) as u64
            }
            ebpf::ARSH64_REG => {
                reg[_dst] = (reg[_dst] as i64 >> (reg[_src] as u64 & SHIFT_MASK_64)) as u64
            }

            // BPF_JMP class
            // TODO: check this actually works as expected for signed / unsigned ops
            ebpf::JA => do_jump(&mut insn_ptr, &insn),
            ebpf::JEQ_IMM => {
                if reg[_dst] == insn.imm as u64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JEQ_REG => {
                if reg[_dst] == reg[_src] {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGT_IMM => {
                if reg[_dst] > insn.imm as u64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGT_REG => {
                if reg[_dst] > reg[_src] {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGE_IMM => {
                if reg[_dst] >= insn.imm as u64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGE_REG => {
                if reg[_dst] >= reg[_src] {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLT_IMM => {
                if reg[_dst] < insn.imm as u64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLT_REG => {
                if reg[_dst] < reg[_src] {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLE_IMM => {
                if reg[_dst] <= insn.imm as u64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLE_REG => {
                if reg[_dst] <= reg[_src] {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSET_IMM => {
                if reg[_dst] & insn.imm as u64 != 0 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSET_REG => {
                if reg[_dst] & reg[_src] != 0 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JNE_IMM => {
                if reg[_dst] != insn.imm as u64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JNE_REG => {
                if reg[_dst] != reg[_src] {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGT_IMM => {
                if reg[_dst] as i64 > insn.imm as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGT_REG => {
                if reg[_dst] as i64 > reg[_src] as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGE_IMM => {
                if reg[_dst] as i64 >= insn.imm as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGE_REG => {
                if reg[_dst] as i64 >= reg[_src] as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLT_IMM => {
                if (reg[_dst] as i64) < insn.imm as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLT_REG => {
                if (reg[_dst] as i64) < reg[_src] as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLE_IMM => {
                if reg[_dst] as i64 <= insn.imm as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLE_REG => {
                if reg[_dst] as i64 <= reg[_src] as i64 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }

            // BPF_JMP32 class
            ebpf::JEQ_IMM32 => {
                if reg[_dst] as u32 == insn.imm as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JEQ_REG32 => {
                if reg[_dst] as u32 == reg[_src] as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGT_IMM32 => {
                if reg[_dst] as u32 > insn.imm as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGT_REG32 => {
                if reg[_dst] as u32 > reg[_src] as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGE_IMM32 => {
                if reg[_dst] as u32 >= insn.imm as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JGE_REG32 => {
                if reg[_dst] as u32 >= reg[_src] as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLT_IMM32 => {
                if (reg[_dst] as u32) < insn.imm as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLT_REG32 => {
                if (reg[_dst] as u32) < reg[_src] as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLE_IMM32 => {
                if reg[_dst] as u32 <= insn.imm as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JLE_REG32 => {
                if reg[_dst] as u32 <= reg[_src] as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSET_IMM32 => {
                if reg[_dst] as u32 & insn.imm as u32 != 0 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSET_REG32 => {
                if reg[_dst] as u32 & reg[_src] as u32 != 0 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JNE_IMM32 => {
                if reg[_dst] as u32 != insn.imm as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JNE_REG32 => {
                if reg[_dst] as u32 != reg[_src] as u32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGT_IMM32 => {
                if reg[_dst] as i32 > insn.imm {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGT_REG32 => {
                if reg[_dst] as i32 > reg[_src] as i32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGE_IMM32 => {
                if reg[_dst] as i32 >= insn.imm {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSGE_REG32 => {
                if reg[_dst] as i32 >= reg[_src] as i32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLT_IMM32 => {
                if (reg[_dst] as i32) < insn.imm {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLT_REG32 => {
                if (reg[_dst] as i32) < reg[_src] as i32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLE_IMM32 => {
                if reg[_dst] as i32 <= insn.imm {
                    do_jump(&mut insn_ptr, &insn);
                }
            }
            ebpf::JSLE_REG32 => {
                if reg[_dst] as i32 <= reg[_src] as i32 {
                    do_jump(&mut insn_ptr, &insn);
                }
            }

            // Do not delegate the check to the verifier, since registered functions can be
            // changed after the program has been verified.
            ebpf::CALL => {
                // See https://www.kernel.org/doc/html/latest/bpf/standardization/instruction-set.html#id16
                let src_reg = _src;
                let call_func_res = match src_reg {
                    0 => {
                        // Handle call by address to external function.
                        if let Some(function) = helpers.get(&(insn.imm as u32)) {
                            reg[0] = function(reg[1], reg[2], reg[3], reg[4], reg[5]);
                            Ok(())
                        }else {
                            Err(format!(
                                "Error: unknown helper function (id: {:#x}) [{}], (instruction #{})",
                                insn.imm as u32,BPF_FUNC_MAPPER[insn.imm as usize],insn_ptr
                            ))
                        }
                    }
                    1 => {
                        // bpf to bpf call
                        // The function is in the same program, so we can just jump to the address
                        if stacks.len() >= ebpf::RBPF_MAX_CALL_DEPTH{
                            Err(format!(
                                "Error: bpf to bpf call stack limit reached (instruction #{}) max depth: {}",
                                insn_ptr, ebpf::RBPF_MAX_CALL_DEPTH
                            ))
                        }else {
                            let mut pre_stack = stacks.last_mut().unwrap();
                            // Save the callee saved registers
                            pre_stack.save_registers(&reg[6..=9]);
                            // Save the return address
                            pre_stack.save_return_address(insn_ptr as u64);
                            // save the stack pointer
                            pre_stack.save_sp(reg[10]);
                            let mut stack = StackFrame::new();
                            log::trace!("BPF TO BPF CALL: new pc: {} + {} = {}",insn_ptr ,insn.imm,insn_ptr + insn.imm as usize);
                            reg[10] = stack.as_ptr() as u64 + stack.len() as u64;
                            stacks.push(stack);
                            insn_ptr += insn.imm as usize;
                            Ok(())
                        }
                    }
                    _ =>{
                        Err(format!(
                            "Error: the function call type (id: {:#x}) [{}], (instruction #{}) not supported",
                            insn.imm as u32,BPF_FUNC_MAPPER[insn.imm as usize],insn_ptr
                        ))
                    }
                };
                if let Err(e) = call_func_res {
                    Err(Error::new(ErrorKind::Other, e))?;
                }
            }
            ebpf::TAIL_CALL => unimplemented!(),
            ebpf::EXIT => {
                if stacks.len() == 1 {
                    return Ok(reg[0]);
                } else {
                    // Pop the stack
                    stacks.pop();
                    let stack = stacks.last().unwrap();
                    // Restore the callee saved registers
                    reg[6..=9].copy_from_slice(&stack.get_registers());
                    // Restore the return address
                    insn_ptr = stack.get_return_address() as usize;
                    // Restore the stack pointer
                    reg[10] = stack.get_sp();
                    log::trace!("EXIT: new pc: {}", insn_ptr);
                }
            }

            _ => unreachable!(),
        }
    }

    unreachable!()
}
