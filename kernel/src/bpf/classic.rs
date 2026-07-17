//! Classic BPF (cBPF) 解释器与验证器。
//!
//! 本模块只实现 cBPF 机器模型、结构验证和输入协议。packet 与 seccomp
//! 的字节序、逻辑偏移和 ancillary 语义由各自的输入实现负责。

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

/// Linux socket filter ancillary 偏移。
pub const SKF_AD_OFF: u32 = (-0x1000i32) as u32;
pub const SKF_AD_PROTOCOL: u32 = 0;
pub const SKF_AD_PKTTYPE: u32 = 4;
pub const SKF_AD_IFINDEX: u32 = 8;
pub const SKF_AD_NLATTR: u32 = 12;
pub const SKF_AD_NLATTR_NEST: u32 = 16;
pub const SKF_AD_MARK: u32 = 20;
pub const SKF_AD_QUEUE: u32 = 24;
pub const SKF_AD_HATYPE: u32 = 28;
pub const SKF_AD_RXHASH: u32 = 32;
pub const SKF_AD_CPU: u32 = 36;
pub const SKF_AD_ALU_XOR_X: u32 = 40;
pub const SKF_AD_VLAN_TAG: u32 = 44;
pub const SKF_AD_VLAN_TAG_PRESENT: u32 = 48;
pub const SKF_AD_PAY_OFFSET: u32 = 52;
pub const SKF_AD_RANDOM: u32 = 56;
pub const SKF_AD_VLAN_TPID: u32 = 60;
pub const SKF_NET_OFF: i32 = -0x100000;
pub const SKF_LL_OFF: i32 = -0x200000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BpfWidth {
    Word,
    Half,
    Byte,
}

/// cBPF 的调用域输入。实现必须自行定义普通加载的字节序和负偏移语义。
pub trait ClassicBpfInput {
    fn len(&self) -> u32;

    fn load(&self, offset: i32, width: BpfWidth) -> Option<u32>;

    fn load_ancillary(&self, extension: u32, accumulator: u32, index: u32) -> Option<u32> {
        let _ = (extension, accumulator, index);
        None
    }
}

impl ClassicBpfInput for [u8] {
    #[inline]
    fn len(&self) -> u32 {
        self.len().min(u32::MAX as usize) as u32
    }

    #[inline]
    fn load(&self, offset: i32, width: BpfWidth) -> Option<u32> {
        let offset = usize::try_from(offset).ok()?;
        match width {
            BpfWidth::Word => {
                let bytes = self.get(offset..offset.checked_add(4)?)?;
                Some(u32::from_be_bytes(bytes.try_into().ok()?))
            }
            BpfWidth::Half => {
                let bytes = self.get(offset..offset.checked_add(2)?)?;
                Some(u16::from_be_bytes(bytes.try_into().ok()?) as u32)
            }
            BpfWidth::Byte => self.get(offset).copied().map(u32::from),
        }
    }
}

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
/// - `BPF_LD|BPF_W/H/B|BPF_ABS`: 由 input 按调用域语义加载
/// - `BPF_LD|*|BPF_IND`: 同上，但 offset = X + k
/// - 越界读取（offset + size > data.len()）：立即返回 0
/// - `BPF_LEN`: A = data.len() as u32
///
/// # 返回值
/// - 正常返回 `RET` 指令的值（返回截断长度，0 = 丢弃）
/// - `DIV`/`MOD` 除数为 0：立即返回 0
/// - fall-through（无 RET 结尾）：返回 0（丢弃）
pub fn run_cbpf<I: ClassicBpfInput + ?Sized>(insns: &[SockFilter], input: &I) -> u32 {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut pc: usize = 0;
    let mut mem = [0u32; BPF_MEMWORDS];

    while pc < insns.len() {
        let insn = &insns[pc];
        if is_ancillary(insn.k) && matches!(insn.code, 0x20 | 0x28 | 0x30) {
            let extension = insn.k.wrapping_sub(SKF_AD_OFF);
            if extension == SKF_AD_ALU_XOR_X {
                a ^= x;
            } else {
                a = match input.load_ancillary(extension, a, x) {
                    Some(value) => value,
                    None => return 0,
                };
            }
            pc += 1;
            continue;
        }

        match insn.code {
            0x00 => a = insn.k, // LD IMM
            0x01 => x = insn.k, // LDX IMM
            0x20 | 0x28 | 0x30 => {
                a = match input.load(insn.k as i32, width(insn.code)) {
                    Some(value) => value,
                    None => return 0,
                };
            }
            0x40 | 0x48 | 0x50 => {
                let offset = x.wrapping_add(insn.k) as i32;
                a = match input.load(offset, width(insn.code)) {
                    Some(value) => value,
                    None => return 0,
                };
            }
            0x60 => {
                a = match mem.get(insn.k as usize) {
                    Some(value) => *value,
                    None => return 0,
                }
            }
            0x61 => {
                x = match mem.get(insn.k as usize) {
                    Some(value) => *value,
                    None => return 0,
                }
            }
            0x80 => a = input.len(),
            0x81 => x = input.len(),
            0xb1 => {
                x = match input.load(insn.k as i32, BpfWidth::Byte) {
                    Some(value) => (value & 0xf) << 2,
                    None => return 0,
                };
            }
            0x02 => match mem.get_mut(insn.k as usize) {
                Some(slot) => *slot = a,
                None => return 0,
            },
            0x03 => match mem.get_mut(insn.k as usize) {
                Some(slot) => *slot = x,
                None => return 0,
            },
            0x04 => a = a.wrapping_add(insn.k),
            0x0c => a = a.wrapping_add(x),
            0x14 => a = a.wrapping_sub(insn.k),
            0x1c => a = a.wrapping_sub(x),
            0x24 => a = a.wrapping_mul(insn.k),
            0x2c => a = a.wrapping_mul(x),
            0x34 => {
                if insn.k == 0 {
                    return 0;
                }
                a /= insn.k;
            }
            0x3c => {
                if x == 0 {
                    return 0;
                }
                a /= x;
            }
            0x44 => a |= insn.k,
            0x4c => a |= x,
            0x54 => a &= insn.k,
            0x5c => a &= x,
            0x64 => a = a.wrapping_shl(insn.k),
            0x6c => a = a.wrapping_shl(x & 31),
            0x74 => a = a.wrapping_shr(insn.k),
            0x7c => a = a.wrapping_shr(x & 31),
            0x84 => a = a.wrapping_neg(),
            0x94 => {
                if insn.k == 0 {
                    return 0;
                }
                a %= insn.k;
            }
            0x9c => {
                if x == 0 {
                    return 0;
                }
                a %= x;
            }
            0xa4 => a ^= insn.k,
            0xac => a ^= x,
            0x05 => {
                pc = match pc
                    .checked_add(1)
                    .and_then(|next| next.checked_add(insn.k as usize))
                {
                    Some(target) => target,
                    None => return 0,
                };
                continue;
            }
            0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d | 0x45 | 0x4d => {
                let value = if insn.code & BPF_X == 0 { insn.k } else { x };
                let branch = match insn.code & 0xf0 {
                    BPF_JEQ => a == value,
                    BPF_JGT => a > value,
                    BPF_JGE => a >= value,
                    BPF_JSET => a & value != 0,
                    _ => return 0,
                };
                pc += 1 + usize::from(if branch { insn.jt } else { insn.jf });
                continue;
            }
            0x06 => return insn.k,
            0x16 => return a,
            0x07 => x = a,
            0x87 => a = x,
            _ => return 0,
        }
        pc += 1;
    }

    // fall-through：返回 0（丢弃）
    0
}

#[inline]
fn width(code: u16) -> BpfWidth {
    match code & 0x18 {
        BPF_W => BpfWidth::Word,
        BPF_H => BpfWidth::Half,
        BPF_B => BpfWidth::Byte,
        _ => unreachable!(),
    }
}

#[inline]
fn is_ancillary(offset: u32) -> bool {
    offset >= SKF_AD_OFF
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
    if !matches!(insns[insns.len() - 1].code, 0x06 | 0x16) {
        return Err(SystemError::EINVAL);
    }

    for (pc, insn) in insns.iter().enumerate() {
        if !code_allowed(insn.code) {
            return Err(SystemError::EINVAL);
        }

        match insn.code {
            0x34 | 0x94 if insn.k == 0 => return Err(SystemError::EINVAL),
            0x64 | 0x74 if insn.k >= 32 => return Err(SystemError::EINVAL),
            0x60 | 0x61 | 0x02 | 0x03 if insn.k as usize >= BPF_MEMWORDS => {
                return Err(SystemError::EINVAL);
            }
            0x05 => {
                if insn.k >= (insns.len() - pc - 1) as u32 {
                    return Err(SystemError::EINVAL);
                }
            }
            0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d | 0x45 | 0x4d => {
                if pc + insn.jt as usize + 1 >= insns.len()
                    || pc + insn.jf as usize + 1 >= insns.len()
                {
                    return Err(SystemError::EINVAL);
                }
            }
            0x20 | 0x28 | 0x30 if is_ancillary(insn.k) && !known_ancillary(insn.k) => {
                return Err(SystemError::EINVAL);
            }
            _ => {}
        }
    }

    check_loads_and_stores(insns)
}

#[inline]
fn code_allowed(code: u16) -> bool {
    const ALLOWED: &[u16] = &[
        BPF_ALU | BPF_ADD | BPF_K,
        BPF_ALU | BPF_ADD | BPF_X,
        BPF_ALU | BPF_SUB | BPF_K,
        BPF_ALU | BPF_SUB | BPF_X,
        BPF_ALU | BPF_MUL | BPF_K,
        BPF_ALU | BPF_MUL | BPF_X,
        BPF_ALU | BPF_DIV | BPF_K,
        BPF_ALU | BPF_DIV | BPF_X,
        BPF_ALU | BPF_MOD | BPF_K,
        BPF_ALU | BPF_MOD | BPF_X,
        BPF_ALU | BPF_AND | BPF_K,
        BPF_ALU | BPF_AND | BPF_X,
        BPF_ALU | BPF_OR | BPF_K,
        BPF_ALU | BPF_OR | BPF_X,
        BPF_ALU | BPF_XOR | BPF_K,
        BPF_ALU | BPF_XOR | BPF_X,
        BPF_ALU | BPF_LSH | BPF_K,
        BPF_ALU | BPF_LSH | BPF_X,
        BPF_ALU | BPF_RSH | BPF_K,
        BPF_ALU | BPF_RSH | BPF_X,
        BPF_ALU | BPF_NEG,
        BPF_LD | BPF_W | BPF_ABS,
        BPF_LD | BPF_H | BPF_ABS,
        BPF_LD | BPF_B | BPF_ABS,
        BPF_LD | BPF_W | BPF_LEN,
        BPF_LD | BPF_W | BPF_IND,
        BPF_LD | BPF_H | BPF_IND,
        BPF_LD | BPF_B | BPF_IND,
        BPF_LD | BPF_IMM,
        BPF_LD | BPF_MEM,
        BPF_LDX | BPF_W | BPF_LEN,
        BPF_LDX | BPF_B | BPF_MSH,
        BPF_LDX | BPF_IMM,
        BPF_LDX | BPF_MEM,
        BPF_ST,
        BPF_STX,
        BPF_MISC | BPF_TAX,
        BPF_MISC | BPF_TXA,
        BPF_RET | BPF_K,
        BPF_RET | BPF_A,
        BPF_JMP | BPF_JA,
        BPF_JMP | BPF_JEQ | BPF_K,
        BPF_JMP | BPF_JEQ | BPF_X,
        BPF_JMP | BPF_JGE | BPF_K,
        BPF_JMP | BPF_JGE | BPF_X,
        BPF_JMP | BPF_JGT | BPF_K,
        BPF_JMP | BPF_JGT | BPF_X,
        BPF_JMP | BPF_JSET | BPF_K,
        BPF_JMP | BPF_JSET | BPF_X,
    ];

    ALLOWED.contains(&code)
}

#[inline]
fn known_ancillary(offset: u32) -> bool {
    matches!(
        offset.wrapping_sub(SKF_AD_OFF),
        SKF_AD_PROTOCOL
            | SKF_AD_PKTTYPE
            | SKF_AD_IFINDEX
            | SKF_AD_NLATTR
            | SKF_AD_NLATTR_NEST
            | SKF_AD_MARK
            | SKF_AD_QUEUE
            | SKF_AD_HATYPE
            | SKF_AD_RXHASH
            | SKF_AD_CPU
            | SKF_AD_ALU_XOR_X
            | SKF_AD_VLAN_TAG
            | SKF_AD_VLAN_TAG_PRESENT
            | SKF_AD_PAY_OFFSET
            | SKF_AD_RANDOM
            | SKF_AD_VLAN_TPID
    )
}

fn check_loads_and_stores(insns: &[SockFilter]) -> Result<(), SystemError> {
    let mut masks = Vec::new();
    masks
        .try_reserve_exact(insns.len())
        .map_err(|_| SystemError::ENOMEM)?;
    masks.resize(insns.len(), u16::MAX);
    let mut memvalid = 0u16;

    for (pc, insn) in insns.iter().enumerate() {
        memvalid &= masks[pc];
        match insn.code {
            0x02 | 0x03 => memvalid |= 1 << insn.k,
            0x60 | 0x61 if memvalid & (1 << insn.k) == 0 => {
                return Err(SystemError::EINVAL);
            }
            0x05 => {
                masks[pc + 1 + insn.k as usize] &= memvalid;
                memvalid = u16::MAX;
            }
            0x15 | 0x1d | 0x25 | 0x2d | 0x35 | 0x3d | 0x45 | 0x4d => {
                masks[pc + 1 + insn.jt as usize] &= memvalid;
                masks[pc + 1 + insn.jf as usize] &= memvalid;
                memvalid = u16::MAX;
            }
            _ => {}
        }
    }

    Ok(())
}

// ============ 用户空间解析 ============

/// 从 optval 字节切片解析 `sock_fprog` 结构并读取 filter 指令。
///
/// `optval` 必须精确包含一个 native `struct sock_fprog`。
/// 内部通过 `UserBufferReader` 安全地从用户空间读取 filter 指令数组。
///
/// 返回已解析的 `Vec<SockFilter>`。
pub(crate) fn parse_sock_fprog(optval: &[u8]) -> Result<SockFprog, SystemError> {
    let fprog_size = core::mem::size_of::<SockFprog>();
    if optval.len() != fprog_size {
        return Err(SystemError::EINVAL);
    }

    let len = u16::from_ne_bytes([optval[0], optval[1]]);
    let filter = u64::from_ne_bytes([
        optval[8], optval[9], optval[10], optval[11], optval[12], optval[13], optval[14],
        optval[15],
    ]);

    Ok(SockFprog {
        len,
        _pad: [0; 6],
        filter,
    })
}

pub(crate) fn read_sock_fprog_insns(fprog: &SockFprog) -> Result<Vec<SockFilter>, SystemError> {
    if fprog.len == 0 || fprog.len as usize > BPF_MAXINSNS {
        return Err(SystemError::EINVAL);
    }

    read_filter_insns(fprog.filter, fprog.len as usize)
}

pub(crate) fn read_sock_fprog(optval: &[u8]) -> Result<Vec<SockFilter>, SystemError> {
    let fprog = parse_sock_fprog(optval)?;
    read_sock_fprog_insns(&fprog)
}

/// 从用户空间读取 filter 指令数组。
fn read_filter_insns(ptr: u64, count: usize) -> Result<Vec<SockFilter>, SystemError> {
    let insn_size = core::mem::size_of::<SockFilter>();
    let byte_len = count.checked_mul(insn_size).ok_or(SystemError::EINVAL)?;

    let mut buf = Vec::new();
    buf.try_reserve_exact(byte_len)
        .map_err(|_| SystemError::ENOMEM)?;
    buf.resize(byte_len, 0);
    let reader = UserBufferReader::new(ptr as *const u8, byte_len, true)?;
    reader.copy_from_user_protected(&mut buf, 0)?;

    let mut insns = Vec::new();
    insns
        .try_reserve_exact(count)
        .map_err(|_| SystemError::ENOMEM)?;
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
