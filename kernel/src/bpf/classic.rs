//! Classic BPF (cBPF) 解释器与验证器。
//!
//! 本模块从 `process/seccomp.rs` 提取，泛化为接收 `&[u8]` 数据输入，
//! 同时服务于 seccomp 系统调用过滤和 AF_PACKET 包过滤。
//!
//! 数据加载语义：BPF_W / BPF_H 使用 **大端（网络字节序）** 读取，
//! 与 Linux `bpf_internal_load_pointer_positive_helper` 中的
//! `get_unaligned_be16` / `get_unaligned_be32` 一致。
//! seccomp 调用方需将 `SeccompData` 的每个字段以大端序写入字节缓冲区。

use alloc::vec::Vec;
use system_error::SystemError;

use crate::syscall::user_access::UserBufferReader;

// ============ Sock Filter / Sock Fprog ============

/// Classic BPF 指令（对应 Linux `struct sock_filter`，8 字节）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

/// `struct sock_fprog` —— 用户空间传入的 filter 容器。
///
/// 布局：`{ u16 len; u8 _pad[6]; u64 filter; }`（共 16 字节）。
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockFprog {
    pub len: u16,
    pub _pad: [u8; 6],
    pub filter: u64,
}

// ============ BPF 指令常量 ============
// 参考 Linux include/uapi/linux/filter.h

/// 指令类（code & 0x07）
pub const BPF_LD: u16 = 0x00;
pub const BPF_LDX: u16 = 0x01;
pub const BPF_ST: u16 = 0x02;
pub const BPF_STX: u16 = 0x03;
pub const BPF_ALU: u16 = 0x04;
pub const BPF_JMP: u16 = 0x05;
pub const BPF_RET: u16 = 0x06;
pub const BPF_MISC: u16 = 0x07;

/// 加载宽度（code & 0x18，仅对 LD/LDX 有意义）
pub const BPF_W: u16 = 0x00;
pub const BPF_H: u16 = 0x08;
pub const BPF_B: u16 = 0x10;

/// 寻址模式（code & 0xe0，仅对 LD/LDX 有意义）
pub const BPF_IMM: u16 = 0x00;
pub const BPF_ABS: u16 = 0x20;
pub const BPF_IND: u16 = 0x40;
pub const BPF_MEM: u16 = 0x60;
pub const BPF_LEN: u16 = 0x80;
/// BPF_MSH：仅用于 `LDX|BPF_B|BPF_MSH`，提取 IP 头长度
/// （`x = (data[k] & 0xf) << 2`）。
pub const BPF_MSH: u16 = 0xa0;

/// 数据源（code & 0x08，仅对 ALU/JMP 有意义）
pub const BPF_K: u16 = 0x00;
pub const BPF_X: u16 = 0x08;
pub const BPF_A: u16 = 0x10;

/// RET 返回值掩码
pub const BPF_RVAL_MASK: u16 = 0x18;

/// ALU 操作（code & 0xf0）
pub const BPF_ADD: u16 = 0x00;
pub const BPF_SUB: u16 = 0x10;
pub const BPF_MUL: u16 = 0x20;
pub const BPF_DIV: u16 = 0x30;
pub const BPF_OR: u16 = 0x40;
pub const BPF_AND: u16 = 0x50;
pub const BPF_LSH: u16 = 0x60;
pub const BPF_RSH: u16 = 0x70;
pub const BPF_NEG: u16 = 0x80;
pub const BPF_MOD: u16 = 0x90;
pub const BPF_XOR: u16 = 0xa0;

/// JMP 操作（code & 0xf0）
pub const BPF_JA: u16 = 0x00;
pub const BPF_JEQ: u16 = 0x10;
pub const BPF_JGT: u16 = 0x20;
pub const BPF_JGE: u16 = 0x30;
pub const BPF_JSET: u16 = 0x40;

/// MISC 操作（code & 0xf8）
pub const BPF_TAX: u16 = 0x00;
pub const BPF_TXA: u16 = 0x80;

/// BPF 程序最大指令数（Linux 限制）
pub const BPF_MAXINSNS: usize = 4096;

/// BPF 记忆体大小（16 个 u32 字）
pub const BPF_MEMWORDS: usize = 16;

// ============ cBPF 解释器 ============

/// 运行 classic BPF 程序。
///
/// cBPF 寄存器模型：
/// - A (u32): 累加器
/// - X (u32): 索引寄存器
/// - pc: 程序计数器
/// - mem\[16\]: 记忆体
///
/// # 加载语义
/// - `BPF_LD|BPF_W|BPF_ABS`: 读 `data[k..k+4]` → u32（**大端**）
/// - `BPF_LD|BPF_H|BPF_ABS`: 读 `data[k..k+2]` → u16（**大端**）
/// - `BPF_LD|BPF_B|BPF_ABS`: 读 `data[k..k+1]` → u8
/// - `BPF_LD|*|BPF_IND`: 同上，但 offset = X + k
/// - 越界读取（offset + size > data.len()）：A = 0
/// - `BPF_LEN`: A = data.len() as u32
///
/// # 返回值
/// - 正常返回 `RET` 指令的值（返回截断长度，0 = 丢弃）
/// - `DIV`/`MOD` 除数为 0：A = 0（继续执行）
/// - fall-through（无 RET 结尾）：返回 0（丢弃）
pub fn run_cbpf(insns: &[SockFilter], data: &[u8]) -> u32 {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut pc: usize = 0;
    let mut mem = [0u32; BPF_MEMWORDS];

    while pc < insns.len() {
        let insn = &insns[pc];
        let class = insn.code & 0x07;

        match class {
            BPF_LD => {
                let mode = insn.code & 0xe0;
                match mode {
                    BPF_IMM => a = insn.k,
                    BPF_ABS | BPF_IND => {
                        let base = if mode == BPF_ABS {
                            insn.k as usize
                        } else {
                            (x as usize).wrapping_add(insn.k as usize)
                        };
                        a = load_packet(data, base, insn.code);
                    }
                    BPF_MEM => {
                        if (insn.k as usize) < BPF_MEMWORDS {
                            a = mem[insn.k as usize];
                        }
                    }
                    BPF_LEN => a = data.len() as u32,
                    _ => {}
                }
                pc += 1;
            }
            BPF_LDX => {
                let mode = insn.code & 0xe0;
                match mode {
                    BPF_IMM => x = insn.k,
                    BPF_MEM => {
                        if (insn.k as usize) < BPF_MEMWORDS {
                            x = mem[insn.k as usize];
                        }
                    }
                    BPF_LEN => x = data.len() as u32,
                    // 提取 IP 头长度：x = (data[k] & 0xf) << 2；越界时 x = 0
                    BPF_MSH => {
                        x = data
                            .get(insn.k as usize)
                            .map(|&b| ((b & 0xf) as u32) << 2)
                            .unwrap_or(0);
                    }
                    _ => {}
                }
                pc += 1;
            }
            BPF_ST => {
                if (insn.k as usize) < BPF_MEMWORDS {
                    mem[insn.k as usize] = a;
                }
                pc += 1;
            }
            BPF_STX => {
                if (insn.k as usize) < BPF_MEMWORDS {
                    mem[insn.k as usize] = x;
                }
                pc += 1;
            }
            BPF_ALU => {
                let op = insn.code & 0xf0;
                let src = insn.code & 0x08;
                let val = if src == BPF_K { insn.k } else { x };
                match op {
                    BPF_ADD => a = a.wrapping_add(val),
                    BPF_SUB => a = a.wrapping_sub(val),
                    BPF_MUL => a = a.wrapping_mul(val),
                    BPF_DIV => a = a.checked_div(val).unwrap_or(0),
                    BPF_OR => a |= val,
                    BPF_AND => a &= val,
                    BPF_LSH => a = a.wrapping_shl(val),
                    BPF_RSH => a = a.wrapping_shr(val),
                    BPF_NEG => a = a.wrapping_neg(),
                    BPF_MOD => a = a.checked_rem(val).unwrap_or(0),
                    BPF_XOR => a ^= val,
                    _ => {}
                }
                pc += 1;
            }
            BPF_JMP => {
                let op = insn.code & 0xf0;
                if op == BPF_JA {
                    pc = match pc
                        .checked_add(1)
                        .and_then(|v| v.checked_add(insn.k as usize))
                    {
                        Some(target) => target,
                        None => return 0,
                    };
                } else {
                    let src = insn.code & 0x08;
                    let val = if src == BPF_K { insn.k } else { x };
                    let cond = match op {
                        BPF_JEQ => a == val,
                        BPF_JGT => a > val,
                        BPF_JGE => a >= val,
                        BPF_JSET => (a & val) != 0,
                        _ => false,
                    };
                    if cond {
                        pc += 1 + insn.jt as usize;
                    } else {
                        pc += 1 + insn.jf as usize;
                    }
                }
            }
            BPF_RET => {
                let rval = insn.code & BPF_RVAL_MASK;
                return if rval == BPF_K { insn.k } else { a };
            }
            BPF_MISC => {
                let op = insn.code & 0xf8;
                match op {
                    BPF_TAX => x = a,
                    BPF_TXA => a = x,
                    _ => {}
                }
                pc += 1;
            }
            _ => {
                pc += 1;
            }
        }
    }

    // fall-through：返回 0（丢弃）
    0
}

/// 按 size 字段从 packet 数据加载一个 u32 值。
///
/// 越界时返回 0。大端读取，与 Linux
/// `get_unaligned_be16` / `get_unaligned_be32` 一致。
#[inline]
fn load_packet(data: &[u8], offset: usize, code: u16) -> u32 {
    let size_code = code & 0x18;
    match size_code {
        BPF_W => {
            if let Some(slice) = data.get(offset..offset + 4) {
                u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]])
            } else {
                0
            }
        }
        BPF_H => {
            if let Some(slice) = data.get(offset..offset + 2) {
                u16::from_be_bytes([slice[0], slice[1]]) as u32
            } else {
                0
            }
        }
        BPF_B => data.get(offset).map(|&b| b as u32).unwrap_or(0),
        _ => 0,
    }
}

// ============ cBPF 验证器 ============

/// 验证 cBPF 程序的合法性（通用检查）。
///
/// 检查项：
/// 1. 非空
/// 2. len <= BPF_MAXINSNS (4096)
/// 3. 末条指令 class == BPF_RET
/// 4. 跳转目标 `checked_add`（溢出 → EINVAL），且 < insns.len()
/// 5. ST/STX 的 k < BPF_MEMWORDS (16)
/// 6. ALU|DIV|K 的 k != 0；ALU|MOD|K 的 k != 0（静态拒绝）
/// 7. opcode class 在白名单内（LD 允许 IMM/ABS/IND/MEM/LEN；LDX 允许 IMM/MEM/LEN/MSH，其中 MSH 仅限 BPF_B 宽度）
pub fn validate_cbpf(insns: &[SockFilter]) -> Result<(), SystemError> {
    if insns.is_empty() {
        return Err(SystemError::EINVAL);
    }
    if insns.len() > BPF_MAXINSNS {
        return Err(SystemError::EINVAL);
    }
    if (insns[insns.len() - 1].code & 0x07) != BPF_RET {
        return Err(SystemError::EINVAL);
    }

    for (pc, insn) in insns.iter().enumerate() {
        let class = insn.code & 0x07;
        match class {
            BPF_LD => {
                let mode = insn.code & 0xe0;
                match mode {
                    BPF_IMM | BPF_ABS | BPF_IND | BPF_LEN => {}
                    BPF_MEM => {
                        if insn.k as usize >= BPF_MEMWORDS {
                            return Err(SystemError::EINVAL);
                        }
                    }
                    _ => return Err(SystemError::EINVAL),
                }
            }
            BPF_LDX => {
                let mode = insn.code & 0xe0;
                match mode {
                    BPF_IMM | BPF_LEN => {}
                    BPF_MEM => {
                        if insn.k as usize >= BPF_MEMWORDS {
                            return Err(SystemError::EINVAL);
                        }
                    }
                    // BPF_MSH 仅允许 BPF_B 宽度（提取 IP 头 IHL 字段）
                    BPF_MSH => {
                        if insn.code & 0x18 != BPF_B {
                            return Err(SystemError::EINVAL);
                        }
                    }
                    _ => return Err(SystemError::EINVAL),
                }
            }
            BPF_ST | BPF_STX => {
                if insn.k as usize >= BPF_MEMWORDS {
                    return Err(SystemError::EINVAL);
                }
            }
            BPF_ALU => {
                let op = insn.code & 0xf0;
                let src = insn.code & 0x08;
                match op {
                    BPF_ADD | BPF_SUB | BPF_MUL | BPF_OR | BPF_AND | BPF_LSH | BPF_RSH
                    | BPF_NEG | BPF_XOR => {}
                    BPF_DIV | BPF_MOD => {
                        if src == BPF_K && insn.k == 0 {
                            return Err(SystemError::EINVAL);
                        }
                    }
                    _ => return Err(SystemError::EINVAL),
                }
                // 源只允许 K 或 X
                if src != BPF_K && src != BPF_X {
                    return Err(SystemError::EINVAL);
                }
            }
            BPF_JMP => {
                let op = insn.code & 0xf0;
                if op == BPF_JA {
                    let Some(target) = pc
                        .checked_add(1)
                        .and_then(|v| v.checked_add(insn.k as usize))
                    else {
                        return Err(SystemError::EINVAL);
                    };
                    if target >= insns.len() {
                        return Err(SystemError::EINVAL);
                    }
                } else {
                    match op {
                        BPF_JEQ | BPF_JGT | BPF_JGE | BPF_JSET => {}
                        _ => return Err(SystemError::EINVAL),
                    }
                    let src = insn.code & 0x08;
                    if src != BPF_K && src != BPF_X {
                        return Err(SystemError::EINVAL);
                    }
                    let Some(jt_target) = pc
                        .checked_add(1)
                        .and_then(|v| v.checked_add(insn.jt as usize))
                    else {
                        return Err(SystemError::EINVAL);
                    };
                    let Some(jf_target) = pc
                        .checked_add(1)
                        .and_then(|v| v.checked_add(insn.jf as usize))
                    else {
                        return Err(SystemError::EINVAL);
                    };
                    if jt_target >= insns.len() || jf_target >= insns.len() {
                        return Err(SystemError::EINVAL);
                    }
                }
            }
            BPF_RET => {
                let rval = insn.code & BPF_RVAL_MASK;
                if rval != BPF_K && rval != BPF_A {
                    return Err(SystemError::EINVAL);
                }
            }
            BPF_MISC => {
                let op = insn.code & 0xf8;
                if op != BPF_TAX && op != BPF_TXA {
                    return Err(SystemError::EINVAL);
                }
            }
            _ => return Err(SystemError::EINVAL),
        }
    }

    Ok(())
}

// ============ 用户空间解析 ============

/// 从 optval 字节切片解析 `sock_fprog` 结构并读取 filter 指令。
///
/// `optval` 应包含 `struct sock_fprog` 的原始字节（至少 16 字节）。
/// 内部通过 `UserBufferReader` 安全地从用户空间读取 filter 指令数组。
///
/// 返回已解析的 `Vec<SockFilter>`。
pub(crate) fn read_sock_fprog(optval: &[u8]) -> Result<Vec<SockFilter>, SystemError> {
    let fprog_size = core::mem::size_of::<SockFprog>();
    if optval.len() < fprog_size {
        return Err(SystemError::EINVAL);
    }

    let len = u16::from_ne_bytes([optval[0], optval[1]]);
    let filter = u64::from_ne_bytes([
        optval[8], optval[9], optval[10], optval[11], optval[12], optval[13], optval[14],
        optval[15],
    ]);

    if len == 0 || len as usize > BPF_MAXINSNS {
        return Err(SystemError::EINVAL);
    }

    read_filter_insns(filter, len as usize)
}

/// 从用户空间读取 filter 指令数组。
fn read_filter_insns(ptr: u64, count: usize) -> Result<Vec<SockFilter>, SystemError> {
    let insn_size = core::mem::size_of::<SockFilter>();
    let byte_len = count.checked_mul(insn_size).ok_or(SystemError::EINVAL)?;

    let mut buf = alloc::vec![0u8; byte_len];
    let reader = UserBufferReader::new(ptr as *const u8, byte_len, true)?;
    reader.copy_from_user_protected(&mut buf, 0)?;

    let mut insns = Vec::with_capacity(count);
    for chunk in buf.chunks_exact(insn_size) {
        insns.push(SockFilter {
            code: u16::from_ne_bytes([chunk[0], chunk[1]]),
            jt: chunk[2],
            jf: chunk[3],
            k: u32::from_ne_bytes([chunk[4], chunk[5], chunk[6], chunk[7]]),
        });
    }

    Ok(insns)
}
