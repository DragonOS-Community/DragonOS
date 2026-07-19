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

use crate::bpf::classic::{self, BpfWidth, ClassicBpfInput, SockFilter};
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
        let input = SeccompBpfInput(data);
        let result = classic::run_cbpf(&self.insns, &input);

        // 如果启用了 log 且结果不是 ALLOW，记录日志
        if self.log && (result & SECCOMP_RET_ACTION_FULL) != SECCOMP_RET_ALLOW {
            log::info!("seccomp: filter log: syscall={} ret={:#x}", data.nr, result);
        }

        result
    }
}

struct SeccompBpfInput<'a>(&'a SeccompData);

impl ClassicBpfInput for SeccompBpfInput<'_> {
    #[inline]
    fn len(&self) -> u32 {
        SECCOMP_DATA_SIZE as u32
    }

    fn load(&self, offset: i32, width: BpfWidth) -> Option<u32> {
        if width != BpfWidth::Word || offset < 0 || offset & 3 != 0 {
            return None;
        }

        let offset = offset as usize;
        match offset {
            0 => Some(self.0.nr as u32),
            4 => Some(self.0.arch),
            8 | 12 => Some(native_u64_word(
                self.0.instruction_pointer,
                (offset - 8) / 4,
            )),
            16..SECCOMP_DATA_SIZE => {
                let relative = offset - 16;
                let arg = *self.0.args.get(relative / 8)?;
                Some(native_u64_word(arg, (relative % 8) / 4))
            }
            _ => None,
        }
    }
}

#[inline]
fn native_u64_word(value: u64, index: usize) -> u32 {
    #[cfg(target_endian = "little")]
    let shift = index * 32;
    #[cfg(target_endian = "big")]
    let shift = (1 - index) * 32;
    (value >> shift) as u32
}

// ============ BPF 验证器 ============

/// Validate the legality of a seccomp BPF program.
///
/// First calls the generic `validate_cbpf` for structural checks, then applies
/// seccomp-specific restrictions:
/// 1. LD/LDX only allow BPF_W width (seccomp does not use H/B)
/// 2. BPF_IND indexed loads are not allowed
/// 3. BPF_ABS offsets must be within the SeccompData range and 4-byte aligned
fn validate_seccomp_filter(insns: &[SockFilter]) -> Result<(), SystemError> {
    classic::validate_cbpf(insns)?;

    for insn in insns {
        match insn.code {
            0x20 => {
                let offset = insn.k as usize;
                if !offset.is_multiple_of(4) || offset.saturating_add(4) > SECCOMP_DATA_SIZE {
                    return Err(SystemError::EINVAL);
                }
            }
            // Matches the explicit opcode whitelist in Linux's seccomp_check_filter.
            // Notably, generic socket cBPF accepts MOD, but the seccomp ABI does not
            // accept MOD K/X.
            0x06 | 0x16 | 0x04 | 0x0c | 0x14 | 0x1c | 0x24 | 0x2c | 0x34 | 0x3c | 0x54 | 0x5c
            | 0x44 | 0x4c | 0xa4 | 0xac | 0x64 | 0x6c | 0x74 | 0x7c | 0x84 | 0x00 | 0x01 | 0x80
            | 0x81 | 0x60 | 0x61 | 0x02 | 0x03 | 0x07 | 0x87 | 0x05 | 0x15 | 0x1d | 0x35 | 0x3d
            | 0x25 | 0x2d | 0x45 | 0x4d => {}
            _ => return Err(SystemError::EINVAL),
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

    // Read the sock_fprog structure from userspace (16 bytes)
    let fprog_size = core::mem::size_of::<classic::SockFprog>();
    let mut fprog_buf = [0u8; core::mem::size_of::<classic::SockFprog>()];
    let reader = UserBufferReader::new(fprog_ptr as *const u8, fprog_size, true)?;
    reader.copy_from_user_protected(&mut fprog_buf, 0)?;

    // Parse and read filter instructions
    let insns = classic::read_sock_fprog(&fprog_buf)?;

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
