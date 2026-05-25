//! Seccomp (Secure Computing) 实现
//!
//! 提供基于 classic BPF (cBPF) 的系统调用过滤。
//! 支持 SECCOMP_MODE_STRICT 和 SECCOMP_MODE_FILTER。
//!
//! 参考: Linux 6.6 kernel/seccomp.c

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::Ordering;

use log::warn;
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, ipc::signal::Signal},
    ipc::signal_types::{SaHandlerType, SigCode, SigInfo, SigType, SigactionType, SignalFlags},
    libs::spinlock::SpinLock,
    process::{pid::PidType, ProcessControlBlock, ProcessManager},
    syscall::user_access::UserBufferReader,
};

// ============ Seccomp Return Actions ============
// 参考 Linux include/uapi/linux/seccomp.h

/// 杀死整个进程（线程组）
pub const SECCOMP_RET_KILL_PROCESS: u32 = 0x80000000;
/// 杀死当前线程
pub const SECCOMP_RET_KILL_THREAD: u32 = 0x00000000;
/// 发送 SIGSYS
pub const SECCOMP_RET_TRAP: u32 = 0x00030000;
/// 返回 -errno
pub const SECCOMP_RET_ERRNO: u32 = 0x00050000;
/// 通知 ptracer；无 ptracer 时返回 -ENOSYS
pub const SECCOMP_RET_TRACE: u32 = 0x7ff00000;
/// 允许但记录日志
pub const SECCOMP_RET_LOG: u32 = 0x7ffc0000;
/// 允许
pub const SECCOMP_RET_ALLOW: u32 = 0x7fff0000;
/// 动作掩码（高16位）
pub const SECCOMP_RET_ACTION_FULL: u32 = 0xffff0000;
/// 数据掩码（低16位）
pub const SECCOMP_RET_DATA: u32 = 0x0000ffff;

// ============ Seccomp Mode ============

/// Seccomp 模式
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompMode {
    /// 未启用
    Disabled = 0,
    /// 严格模式：仅允许 read/write/exit/rt_sigreturn
    Strict = 1,
    /// 过滤模式：使用 BPF 过滤器
    Filter = 2,
    /// 进程已被 seccomp 杀死（不可逆）
    Dead = 3,
}

impl From<u8> for SeccompMode {
    fn from(v: u8) -> Self {
        match v {
            0 => Self::Disabled,
            1 => Self::Strict,
            2 => Self::Filter,
            3 => Self::Dead,
            _ => Self::Disabled,
        }
    }
}

// ============ Seccomp Data ============

/// BPF 过滤器执行时的输入数据
/// 对应 Linux struct seccomp_data（64 字节）
#[repr(C)]
#[derive(Debug, Clone)]
pub struct SeccompData {
    /// 系统调用号
    pub nr: i32,
    /// 架构标识（AUDIT_ARCH_*）
    pub arch: u32,
    /// 用户态指令指针
    pub instruction_pointer: u64,
    /// 系统调用参数
    pub args: [u64; 6],
}

const SECCOMP_DATA_SIZE: usize = core::mem::size_of::<SeccompData>();

const MAX_ERRNO: u32 = SystemError::MAXERRNO as u32;
const AUDIT_ARCH_64BIT: u32 = 0x80000000;
const AUDIT_ARCH_LE: u32 = 0x40000000;
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH_X86_64: u32 = 62 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE;
#[cfg(target_arch = "riscv64")]
const AUDIT_ARCH_RISCV64: u32 = 243 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE;
#[cfg(target_arch = "loongarch64")]
const AUDIT_ARCH_LOONGARCH64: u32 = 258 | AUDIT_ARCH_64BIT | AUDIT_ARCH_LE;

// ============ Sock Filter / Sock Fprog ============

/// Classic BPF 指令（对应 Linux struct sock_filter，8 字节）
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockFilter {
    pub code: u16,
    pub jt: u8,
    pub jf: u8,
    pub k: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SockFprog {
    pub len: u16,
    _pad: [u8; 6],
    pub filter: u64,
}

// ============ BPF 指令常量 ============
// 参考 Linux include/uapi/linux/filter.h

// 指令类型
const BPF_LD: u16 = 0x00;
const BPF_LDX: u16 = 0x01;
const BPF_ST: u16 = 0x02;
const BPF_STX: u16 = 0x03;
const BPF_ALU: u16 = 0x04;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_MISC: u16 = 0x07;

// 操作数宽度
#[allow(dead_code)]
const BPF_W: u16 = 0x00;

// 寻址模式
const BPF_IMM: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_MEM: u16 = 0x60;
const BPF_LEN: u16 = 0x80;

// 数据源
const BPF_K: u16 = 0x00;
const BPF_X: u16 = 0x08;
const BPF_A: u16 = 0x10;
const BPF_RVAL_MASK: u16 = 0x18;

// ALU 操作
const BPF_ADD: u16 = 0x00;
const BPF_SUB: u16 = 0x10;
const BPF_MUL: u16 = 0x20;
const BPF_DIV: u16 = 0x30;
const BPF_OR: u16 = 0x40;
const BPF_AND: u16 = 0x50;
const BPF_LSH: u16 = 0x60;
const BPF_RSH: u16 = 0x70;
const BPF_NEG: u16 = 0x80;
const BPF_MOD: u16 = 0x90;
const BPF_XOR: u16 = 0xa0;

// JMP 操作
const BPF_JA: u16 = 0x00;
const BPF_JEQ: u16 = 0x10;
const BPF_JGT: u16 = 0x20;
const BPF_JGE: u16 = 0x30;
const BPF_JSET: u16 = 0x40;

// MISC 操作
const BPF_TAX: u16 = 0x00;
const BPF_TXA: u16 = 0x80;

/// BPF 程序最大指令数（Linux 限制）
const BPF_MAXINSNS: usize = 4096;

/// BPF 记忆体大小（16 个 u32 字）
const BPF_MEMWORDS: usize = 16;

// ============ Strict 模式白名单 ============

/// SECCOMP_MODE_STRICT 允许的系统调用白名单 (x86_64)
#[cfg(target_arch = "x86_64")]
const SECCOMP_STRICT_WHITELIST: [i32; 4] = [
    0,  // read
    1,  // write
    60, // exit
    15, // rt_sigreturn
];

/// SECCOMP_MODE_STRICT 允许的系统调用白名单 (riscv64)
#[cfg(target_arch = "riscv64")]
const SECCOMP_STRICT_WHITELIST: [i32; 4] = [
    63,  // read
    64,  // write
    93,  // exit
    139, // rt_sigreturn
];

/// SECCOMP_MODE_STRICT 允许的系统调用白名单 (loongarch64)
#[cfg(target_arch = "loongarch64")]
const SECCOMP_STRICT_WHITELIST: [i32; 4] = [
    63,  // read
    64,  // write
    93,  // exit
    139, // rt_sigreturn
];

// ============ Seccomp Filter ============

#[derive(Debug)]
pub struct SeccompFilter {
    log: bool,
    prev: Option<Arc<SeccompFilter>>,
    insns: Vec<SockFilter>,
}

impl SeccompFilter {
    /// 创建新的过滤器并挂载到现有过滤器链头部
    ///
    /// # 参数
    /// - `insns`: BPF 指令数组
    /// - `log`: 是否启用日志
    /// - `prev`: 前一个过滤器（链表尾）
    pub fn new(
        insns: Vec<SockFilter>,
        log: bool,
        prev: Option<Arc<SeccompFilter>>,
    ) -> Result<Self, SystemError> {
        validate_seccomp_filter(&insns)?;
        Ok(Self { log, prev, insns })
    }

    /// 获取前一个过滤器
    #[inline]
    pub fn prev(&self) -> &Option<Arc<SeccompFilter>> {
        &self.prev
    }

    pub fn chain_len(head: &Option<Arc<SeccompFilter>>) -> usize {
        let mut len = 0;
        let mut current = head.clone();
        while let Some(filter) = current {
            len += 1;
            current = filter.prev().clone();
        }
        len
    }

    /// 执行此过滤器
    fn run(&self, data: &SeccompData) -> u32 {
        let result = run_cbpf(&self.insns, data);

        // 如果启用了 log 且结果不是 ALLOW，记录日志
        if self.log && (result & SECCOMP_RET_ACTION_FULL) != SECCOMP_RET_ALLOW {
            log::info!("seccomp: filter log: syscall={} ret={:#x}", data.nr, result);
        }

        result
    }
}

// ============ cBPF 解释器 ============

/// 运行 classic BPF 程序
///
/// cBPF 寄存器模型：
/// - A (u32): 累加器
/// - X (u32): 索引寄存器
/// - pc: 程序计数器
/// - mem[16]: 记忆体
fn run_cbpf(insns: &[SockFilter], data: &SeccompData) -> u32 {
    let mut a: u32 = 0;
    let mut x: u32 = 0;
    let mut pc: usize = 0;
    let mut mem = [0u32; BPF_MEMWORDS];

    // 将 seccomp_data 转为字节切片以支持任意偏移读取
    let data_bytes = unsafe {
        core::slice::from_raw_parts(data as *const SeccompData as *const u8, SECCOMP_DATA_SIZE)
    };

    while pc < insns.len() {
        let insn = &insns[pc];
        let class = insn.code & 0x07;

        match class {
            BPF_LD => {
                let mode = insn.code & 0xe0;
                match mode {
                    BPF_IMM => a = insn.k,
                    BPF_ABS => {
                        let offset = insn.k as usize;
                        if offset + 4 <= SECCOMP_DATA_SIZE {
                            a = u32::from_ne_bytes([
                                data_bytes[offset],
                                data_bytes[offset + 1],
                                data_bytes[offset + 2],
                                data_bytes[offset + 3],
                            ]);
                        }
                        // 越界读取：保持 A 不变（与 Linux 一致）
                    }
                    BPF_MEM => {
                        if (insn.k as usize) < BPF_MEMWORDS {
                            a = mem[insn.k as usize];
                        }
                    }
                    BPF_LEN => a = SECCOMP_DATA_SIZE as u32,
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
                    BPF_LEN => x = SECCOMP_DATA_SIZE as u32,
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
                    // 无条件跳转
                    pc = pc.wrapping_add(1).wrapping_add(insn.k as usize);
                } else {
                    // 条件跳转
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

    // 如果程序没有返回指令（不应该通过验证器），默认 KILL_THREAD
    SECCOMP_RET_KILL_THREAD
}

// ============ BPF 验证器 ============

/// 验证 seccomp BPF 程序的合法性
///
/// 检查项：
/// 1. 所有指令都在 seccomp 允许的子集内
/// 2. 跳转目标不越界
/// 3. BPF_LD|BPF_W|BPF_ABS 的偏移量在 seccomp_data 范围内且 4 字节对齐
/// 4. 内存索引 < 16
fn validate_seccomp_filter(insns: &[SockFilter]) -> Result<(), SystemError> {
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
        match insn.code {
            code if code == (BPF_LD | BPF_W | BPF_ABS) => {
                let offset = insn.k as usize;
                if !offset.is_multiple_of(4) || offset.saturating_add(4) > SECCOMP_DATA_SIZE {
                    return Err(SystemError::EINVAL);
                }
            }
            code if code == (BPF_LD | BPF_W | BPF_LEN) => {}
            code if code == (BPF_LDX | BPF_W | BPF_LEN) => {}
            code if code == (BPF_LD | BPF_IMM) => {}
            code if code == (BPF_LDX | BPF_IMM) => {}
            code if code == (BPF_LD | BPF_MEM) || code == (BPF_LDX | BPF_MEM) => {
                if insn.k as usize >= BPF_MEMWORDS {
                    return Err(SystemError::EINVAL);
                }
            }
            code if code == BPF_ST || code == BPF_STX => {
                if insn.k as usize >= BPF_MEMWORDS {
                    return Err(SystemError::EINVAL);
                }
            }
            code if code == (BPF_ALU | BPF_ADD | BPF_K)
                || code == (BPF_ALU | BPF_ADD | BPF_X)
                || code == (BPF_ALU | BPF_SUB | BPF_K)
                || code == (BPF_ALU | BPF_SUB | BPF_X)
                || code == (BPF_ALU | BPF_MUL | BPF_K)
                || code == (BPF_ALU | BPF_MUL | BPF_X)
                || code == (BPF_ALU | BPF_DIV | BPF_K)
                || code == (BPF_ALU | BPF_DIV | BPF_X)
                || code == (BPF_ALU | BPF_OR | BPF_K)
                || code == (BPF_ALU | BPF_OR | BPF_X)
                || code == (BPF_ALU | BPF_AND | BPF_K)
                || code == (BPF_ALU | BPF_AND | BPF_X)
                || code == (BPF_ALU | BPF_LSH | BPF_K)
                || code == (BPF_ALU | BPF_LSH | BPF_X)
                || code == (BPF_ALU | BPF_RSH | BPF_K)
                || code == (BPF_ALU | BPF_RSH | BPF_X)
                || code == (BPF_ALU | BPF_NEG)
                || code == (BPF_ALU | BPF_XOR | BPF_K)
                || code == (BPF_ALU | BPF_XOR | BPF_X) =>
            {
                if code == (BPF_ALU | BPF_DIV | BPF_K) && insn.k == 0 {
                    return Err(SystemError::EINVAL);
                }
            }
            code if code == (BPF_JMP | BPF_JA) => {
                let Some(target) = pc
                    .checked_add(1)
                    .and_then(|x| x.checked_add(insn.k as usize))
                else {
                    return Err(SystemError::EINVAL);
                };
                if target >= insns.len() {
                    return Err(SystemError::EINVAL);
                }
            }
            code if code == (BPF_JMP | BPF_JEQ | BPF_K)
                || code == (BPF_JMP | BPF_JEQ | BPF_X)
                || code == (BPF_JMP | BPF_JGT | BPF_K)
                || code == (BPF_JMP | BPF_JGT | BPF_X)
                || code == (BPF_JMP | BPF_JGE | BPF_K)
                || code == (BPF_JMP | BPF_JGE | BPF_X)
                || code == (BPF_JMP | BPF_JSET | BPF_K)
                || code == (BPF_JMP | BPF_JSET | BPF_X) =>
            {
                let jt_target = pc + 1 + insn.jt as usize;
                let jf_target = pc + 1 + insn.jf as usize;
                if jt_target >= insns.len() || jf_target >= insns.len() {
                    return Err(SystemError::EINVAL);
                }
            }
            code if code == (BPF_RET | BPF_K) || code == (BPF_RET | BPF_A) => {}
            code if code == (BPF_MISC | BPF_TAX) || code == (BPF_MISC | BPF_TXA) => {}
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
    }

    Ok(())
}

// ============ 过滤器执行 ============

/// 运行所有过滤器，返回最终动作（最小值 = 最严格）
///
/// 遍历 prev 链中的所有过滤器，保留动作值最小的结果。
/// 参考 Linux seccomp_run_filters()
fn seccomp_run_filters(data: &SeccompData, filter: &Option<Arc<SeccompFilter>>) -> u32 {
    let Some(head) = filter else {
        return SECCOMP_RET_ALLOW;
    };

    let mut ret = SECCOMP_RET_ALLOW;
    let mut current: Option<Arc<SeccompFilter>> = Some(head.clone());

    while let Some(f) = current {
        let cur_ret = f.run(data);
        if action_priority(cur_ret) < action_priority(ret) {
            ret = cur_ret;
        }
        current = f.prev().clone();
    }

    ret
}

#[inline(always)]
fn action_priority(ret: u32) -> i32 {
    (ret & SECCOMP_RET_ACTION_FULL) as i32
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeccompDecision {
    Allow,
    Skip(usize),
}

// ============ Secure Computing ============

/// Seccomp 系统调用检查入口
///
/// 在系统调用分发之前调用。根据当前进程的 seccomp 模式执行相应检查。
///
/// # 返回值
/// - `Allow`: 允许继续执行系统调用
/// - `Skip(ret)`: 跳过系统调用并直接返回 `ret`
///
/// 对于 KILL 动作，此函数不会返回，而是按 Linux seccomp 语义终止线程/线程组。
pub fn secure_computing(
    syscall_num: usize,
    args: &[usize; 6],
    frame: &mut TrapFrame,
) -> Result<SeccompDecision, SystemError> {
    let pcb = ProcessManager::current_pcb();
    let mode_val = pcb.seccomp_mode.load(Ordering::Relaxed);
    let mode = SeccompMode::from(mode_val);

    match mode {
        SeccompMode::Disabled => Ok(SeccompDecision::Allow),

        SeccompMode::Dead => {
            // Surviving SECCOMP_RET_KILL_* must be proactively impossible.
            ProcessManager::exit(Signal::SIGKILL as usize);
        }

        SeccompMode::Strict => {
            // 检查白名单
            if SECCOMP_STRICT_WHITELIST.contains(&(syscall_num as i32)) {
                Ok(SeccompDecision::Allow)
            } else {
                kill_current_strict();
            }
        }

        SeccompMode::Filter => {
            let data = SeccompData {
                nr: syscall_num as i32,
                arch: seccomp_arch(),
                instruction_pointer: instruction_pointer(frame),
                args: [
                    args[0] as u64,
                    args[1] as u64,
                    args[2] as u64,
                    args[3] as u64,
                    args[4] as u64,
                    args[5] as u64,
                ],
            };

            let filter_guard = pcb.seccomp_filter.lock();
            let result = seccomp_run_filters(&data, &filter_guard);
            drop(filter_guard);

            let action = result & SECCOMP_RET_ACTION_FULL;
            let data_val = result & SECCOMP_RET_DATA;

            match action {
                SECCOMP_RET_KILL_PROCESS | SECCOMP_RET_KILL_THREAD => {
                    kill_current(action);
                }
                SECCOMP_RET_TRAP => {
                    rollback_syscall(frame, syscall_num);
                    send_seccomp_sigsys(&data, data_val);
                    Ok(SeccompDecision::Skip(frame_syscall_return(frame)))
                }
                SECCOMP_RET_ERRNO => {
                    let errno = data_val.min(MAX_ERRNO);
                    Ok(SeccompDecision::Skip((-(errno as i32)) as usize))
                }
                SECCOMP_RET_TRACE => Ok(SeccompDecision::Skip(
                    SystemError::ENOSYS.to_posix_errno() as usize,
                )),
                SECCOMP_RET_LOG => {
                    log::info!(
                        "seccomp: pid={:?} syscall={} action=LOG",
                        pcb.raw_pid(),
                        syscall_num
                    );
                    Ok(SeccompDecision::Allow)
                }
                SECCOMP_RET_ALLOW => Ok(SeccompDecision::Allow),

                _ => {
                    // 未知动作，默认 KILL
                    kill_current(SECCOMP_RET_KILL_PROCESS);
                }
            }
        }
    }
}

/// 发送 SIGSYS 信号给当前进程（TRAP 动作）
fn send_seccomp_sigsys(data: &SeccompData, errno: u32) {
    let pcb = ProcessManager::current_pcb();
    let sig = Signal::SIGSYS;
    log::debug!(
        "seccomp: SIGSYS trap pid={:?} syscall={} arch={:#x}",
        pcb.raw_pid(),
        data.nr,
        data.arch
    );

    force_current_seccomp_sigsys();

    let mut info = SigInfo::new(
        sig,
        errno as i32,
        SigCode::SysSeccomp,
        SigType::SigSys {
            call_addr: data.instruction_pointer,
            syscall: data.nr,
            arch: data.arch,
        },
    );

    if let Err(e) = sig.send_signal_info_to_pcb(Some(&mut info), pcb.clone(), PidType::PID) {
        warn!(
            "seccomp: failed to send SIGSYS to pid={:?}: {:?}",
            pcb.raw_pid(),
            e
        );
    }
}

fn force_current_seccomp_sigsys() {
    let pcb = ProcessManager::current_pcb();
    let sig = Signal::SIGSYS;

    if let Some(mut action) = pcb.sighand().handler(sig) {
        let blocked = pcb
            .sig_info_irqsave()
            .sig_blocked()
            .contains(sig.into_sigset());
        if blocked || action.is_ignore() {
            action.set_action(SigactionType::SaHandler(SaHandlerType::Default));
            pcb.sighand().set_handler(sig, action);
        }
        if action.is_default() {
            pcb.sighand().flags_remove(SignalFlags::UNKILLABLE);
        }
    }

    {
        let mut siginfo = pcb.sig_info_mut();
        siginfo.sig_block_mut().remove(sig.into_sigset());
        siginfo.saved_sigmask_mut().remove(sig.into_sigset());
    }
    pcb.recalc_sigpending();
}

fn kill_current(action: u32) -> ! {
    match action {
        SECCOMP_RET_KILL_THREAD => kill_current_thread(),
        _ => kill_current_process(),
    }
}

fn kill_current_thread() -> ! {
    let pcb = ProcessManager::current_pcb();
    pcb.seccomp_mode
        .store(SeccompMode::Dead as u8, Ordering::SeqCst);
    ProcessManager::exit(Signal::SIGSYS as usize);
}

fn kill_current_strict() -> ! {
    let pcb = ProcessManager::current_pcb();
    pcb.seccomp_mode
        .store(SeccompMode::Dead as u8, Ordering::SeqCst);
    ProcessManager::exit(Signal::SIGKILL as usize);
}

fn kill_current_process() -> ! {
    let pcb = ProcessManager::current_pcb();
    pcb.seccomp_mode
        .store(SeccompMode::Dead as u8, Ordering::SeqCst);
    ProcessManager::group_exit(Signal::SIGSYS as usize);
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn seccomp_arch() -> u32 {
    AUDIT_ARCH_X86_64
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
fn seccomp_arch() -> u32 {
    AUDIT_ARCH_RISCV64
}

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn seccomp_arch() -> u32 {
    AUDIT_ARCH_LOONGARCH64
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn instruction_pointer(frame: &TrapFrame) -> u64 {
    frame.rip() as u64
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
fn instruction_pointer(frame: &TrapFrame) -> u64 {
    frame.epc as u64
}

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn instruction_pointer(frame: &TrapFrame) -> u64 {
    frame.csr_era as u64
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn rollback_syscall(frame: &mut TrapFrame, _syscall_num: usize) {
    frame.rax = frame.errcode;
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
fn rollback_syscall(frame: &mut TrapFrame, syscall_num: usize) {
    frame.a0 = syscall_num;
}

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn rollback_syscall(frame: &mut TrapFrame, syscall_num: usize) {
    frame.a0 = syscall_num;
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn frame_syscall_return(frame: &TrapFrame) -> usize {
    frame.rax as usize
}

#[cfg(target_arch = "riscv64")]
#[inline(always)]
fn frame_syscall_return(frame: &TrapFrame) -> usize {
    frame.a0
}

#[cfg(target_arch = "loongarch64")]
#[inline(always)]
fn frame_syscall_return(frame: &TrapFrame) -> usize {
    frame.a0
}

// ============ Seccomp 模式操作 ============

/// 设置 strict 模式
///
/// 只能从 Disabled 切换（不可逆）。
pub fn seccomp_set_mode_strict() -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();

    // 只能从 Disabled 切换（不可逆）
    let prev = current.seccomp_mode.compare_exchange(
        SeccompMode::Disabled as u8,
        SeccompMode::Strict as u8,
        Ordering::SeqCst,
        Ordering::SeqCst,
    );

    if prev.is_err() {
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

/// 设置 filter 模式，安装新的 BPF 过滤器
///
/// 要求 CAP_SYS_ADMIN 或 no_new_privs 已设置。
/// 当前模式必须为 Disabled 或 Filter。
///
/// # 参数
/// - `fprog_ptr`: 用户态 sock_fprog 结构指针
/// - `flags`: 安装标志（支持 LOG 和 TSYNC）
pub fn seccomp_set_mode_filter(fprog_ptr: u64, flags: u32) -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();

    // Linux: CAP_SYS_ADMIN 或 no_new_privs
    if !current
        .cred()
        .has_capability(crate::process::cred::CAPFlags::CAP_SYS_ADMIN)
        && current.no_new_privs() == 0
    {
        return Err(SystemError::EACCES);
    }

    // 当前模式不能超过 Filter
    let mode = SeccompMode::from(current.seccomp_mode.load(Ordering::Relaxed));
    if mode != SeccompMode::Disabled && mode != SeccompMode::Filter {
        return Err(SystemError::EINVAL);
    }

    // 解析 flags
    const SECCOMP_FILTER_FLAG_TSYNC: u32 = 1 << 0;
    const SECCOMP_FILTER_FLAG_LOG: u32 = 1 << 1;
    const SUPPORTED_FLAGS: u32 = SECCOMP_FILTER_FLAG_TSYNC | SECCOMP_FILTER_FLAG_LOG;
    if flags & !SUPPORTED_FLAGS != 0 {
        return Err(SystemError::EINVAL);
    }
    let log = (flags & SECCOMP_FILTER_FLAG_LOG) != 0;
    let tsync = (flags & SECCOMP_FILTER_FLAG_TSYNC) != 0;

    // 从用户空间读取 sock_fprog
    let fprog = read_sock_fprog(fprog_ptr)?;

    if fprog.len == 0 || fprog.len as usize > BPF_MAXINSNS {
        return Err(SystemError::EINVAL);
    }

    // 从用户空间读取 filter 指令
    let insns = read_filter_insns(fprog.filter, fprog.len as usize)?;

    // 获取当前 filter 链头作为 prev
    let prev = current.seccomp_filter.lock().clone();

    if tsync {
        seccomp_can_sync_threads(&current, &prev)?;
    }

    // 创建新 filter
    let filter = Arc::new(SeccompFilter::new(insns, log, prev)?);

    // 安装到链头
    *current.seccomp_filter.lock() = Some(filter.clone());

    // 设置模式为 Filter（如果是第一次安装）
    current
        .seccomp_mode
        .store(SeccompMode::Filter as u8, Ordering::SeqCst);

    if tsync {
        seccomp_sync_threads(&current, filter);
    }

    Ok(())
}

pub fn seccomp_get_action_avail(action_ptr: u64) -> Result<(), SystemError> {
    let mut buf = [0u8; core::mem::size_of::<u32>()];
    let reader = UserBufferReader::new(action_ptr as *const u8, buf.len(), true)?;
    reader.copy_from_user_protected(&mut buf, 0)?;
    let action = u32::from_ne_bytes(buf);

    match action {
        SECCOMP_RET_KILL_PROCESS
        | SECCOMP_RET_KILL_THREAD
        | SECCOMP_RET_TRAP
        | SECCOMP_RET_ERRNO
        | SECCOMP_RET_TRACE
        | SECCOMP_RET_LOG
        | SECCOMP_RET_ALLOW => Ok(()),
        _ => Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
    }
}

fn seccomp_can_sync_threads(
    current: &Arc<ProcessControlBlock>,
    current_filter: &Option<Arc<SeccompFilter>>,
) -> Result<(), SystemError> {
    for thread in thread_group_tasks(current) {
        if Arc::ptr_eq(&thread, current) || thread.is_exited() || thread.is_dead() {
            continue;
        }

        match thread.seccomp_mode() {
            SeccompMode::Disabled => continue,
            SeccompMode::Filter => {
                let thread_filter = thread.seccomp_filter.lock().clone();
                if is_filter_ancestor(&thread_filter, current_filter) {
                    continue;
                }
            }
            _ => {}
        }

        return Err(SystemError::EINVAL);
    }

    Ok(())
}

fn seccomp_sync_threads(current: &Arc<ProcessControlBlock>, filter: Arc<SeccompFilter>) {
    for thread in thread_group_tasks(current) {
        if Arc::ptr_eq(&thread, current) || thread.is_exited() || thread.is_dead() {
            continue;
        }

        *thread.seccomp_filter.lock() = Some(filter.clone());
        thread
            .seccomp_mode
            .store(SeccompMode::Filter as u8, Ordering::SeqCst);
        if current.no_new_privs() != 0 {
            thread.set_no_new_privs(true);
        }
    }
}

fn thread_group_tasks(current: &Arc<ProcessControlBlock>) -> Vec<Arc<ProcessControlBlock>> {
    let leader = current
        .threads_read_irqsave()
        .group_leader()
        .unwrap_or_else(|| current.clone());
    let mut tasks = Vec::new();
    tasks.push(leader.clone());

    let weak_tasks = leader.threads_read_irqsave().group_tasks_clone();
    for weak in weak_tasks {
        if let Some(task) = weak.upgrade() {
            tasks.push(task);
        }
    }

    tasks
}

fn is_filter_ancestor(
    descendant: &Option<Arc<SeccompFilter>>,
    ancestor: &Option<Arc<SeccompFilter>>,
) -> bool {
    let Some(ancestor) = ancestor else {
        return false;
    };

    let mut current = descendant.clone();
    while let Some(filter) = current {
        if Arc::ptr_eq(&filter, ancestor) {
            return true;
        }
        current = filter.prev().clone();
    }

    false
}

/// 从用户空间读取 sock_fprog 结构
fn read_sock_fprog(ptr: u64) -> Result<SockFprog, SystemError> {
    let size = core::mem::size_of::<SockFprog>();
    let mut buf = [0u8; core::mem::size_of::<SockFprog>()];

    let reader = UserBufferReader::new(ptr as *const u8, size, true)?;
    reader.copy_from_user_protected(&mut buf, 0)?;

    let len = u16::from_ne_bytes([buf[0], buf[1]]);
    let filter = u64::from_ne_bytes([
        buf[8], buf[9], buf[10], buf[11], buf[12], buf[13], buf[14], buf[15],
    ]);

    Ok(SockFprog {
        len,
        _pad: [0u8; 6],
        filter,
    })
}

/// 从用户空间读取 filter 指令数组
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

/// fork 时复制 seccomp 状态
///
/// - 复制 seccomp_mode（值复制）
/// - 共享 seccomp_filter（Arc::clone）
pub fn copy_seccomp(
    parent_mode: u8,
    parent_filter: &SpinLock<Option<Arc<SeccompFilter>>>,
    child_seccomp_mode: &core::sync::atomic::AtomicU8,
    child_seccomp_filter: &SpinLock<Option<Arc<SeccompFilter>>>,
) {
    // 复制 mode
    child_seccomp_mode.store(parent_mode, Ordering::Relaxed);

    // 共享 filter（Arc::clone 增加引用计数）
    let parent_guard = parent_filter.lock();
    if let Some(ref f) = *parent_guard {
        *child_seccomp_filter.lock() = Some(f.clone());
    }
}
