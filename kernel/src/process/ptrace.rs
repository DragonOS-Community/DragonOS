use super::{
    abi::WaitOption, ExitState, ProcessControlBlock, ProcessFlags, ProcessManager, RawPid,
    PTRACE_RELATION_LOCK,
};
use crate::{
    arch::{
        interrupt::TrapFrame,
        ipc::signal::{SigChildCode, SigFlags, SigSet, Signal},
        CurrentIrqArch, MMArch,
    },
    exception::InterruptArch,
    ipc::signal_types::{SigCode, SigInfo, SigType, SignalFlags},
    mm::{fault, ucontext, MemoryManagementArch, PhysAddr, VirtAddr, VmFaultReason, VmFlags},
    process::{namespace::user_namespace::map_id_up, pid::PidType, KernelStack, ProcessState},
    sched::{schedule, SchedMode},
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    mem::size_of,
    sync::atomic::{fence, Ordering},
};
use system_error::SystemError;

/// 信号递送路径的 ptrace hook：在 `do_signal` 出队信号后、动作查找前调用。
pub fn ptrace_signal(
    pcb: &Arc<ProcessControlBlock>,
    original: Signal,
    info: &mut Option<SigInfo>,
) -> Option<Signal> {
    // SIGKILL 不经过 ptrace_signal（防御性，do_signal 已在 kernel_only 路径处理）。
    if original == Signal::SIGKILL {
        return Some(Signal::SIGKILL);
    }

    // 进入 signal-delivery-stop。ptrace_stop 内部会 fatal 早退、阻塞、唤醒后清理。
    let signr = pcb.ptrace_stop(original as usize, SigChildCode::Trapped, info.as_mut());

    if signr == 0 {
        // tracer 丢弃了信号。
        return None;
    }

    let injected = Signal::from(signr as i32);
    if injected == Signal::INVALID {
        return None;
    }

    // 如果 tracer 改变了信号号，重建 siginfo（来源 SI_USER）。
    if let Some(i) = info {
        if injected as i32 != i.signo_i32() {
            let sender = crate::process::ptrace::ptracer_of(pcb).or_else(|| pcb.real_parent_pcb());
            let sender_vpid = sender
                .as_ref()
                .and_then(|parent| parent.task_pid_nr_ns(PidType::PID, Some(pcb.active_pid_ns())))
                .map(|p| p.data())
                .unwrap_or(0);
            // 跨 user namespace 映射失败时回退到 overflowuid（默认 65534）
            const OVERFLOWUID: u32 = 65534;
            let sender_uid = sender
                .as_ref()
                .map(|p| {
                    let kuid = p.cred().uid.data() as u32;
                    map_id_up(&pcb.cred().user_ns.inner.lock().uid_map, kuid).unwrap_or(OVERFLOWUID)
                })
                .unwrap_or(OVERFLOWUID);
            *i = SigInfo::new(
                injected,
                0,
                SigCode::User,
                SigType::Kill {
                    pid: RawPid(sender_vpid),
                    uid: sender_uid,
                },
            );
        }
    }

    // 若注入的信号被当前掩码阻塞，或有 fatal 信号 pending，则重新入队并返回 None，让 do_signal 继续出队下一个
    let blocked = {
        let g = pcb.sig_info_irqsave();
        g.sig_blocked().contains(injected.into())
    };
    let fatal_pending = Signal::fatal_signal_pending(pcb);
    if blocked || fatal_pending {
        if let Some(i) = info.as_mut() {
            let _ = injected.send_signal_info_to_pcb(Some(i), pcb.clone(), PidType::PID);
        } else {
            let _ = injected.send_signal_info_to_pcb(None, pcb.clone(), PidType::PID);
        }
        return None;
    }

    Some(injected)
}

/// ptrace 系统调用的请求类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum PtraceRequest {
    Traceme = 0,
    Peektext = 1,
    Peekdata = 2,
    Peekuser = 3,
    Poketext = 4,
    Pokedata = 5,
    Pokeuser = 6,
    Cont = 7,
    Kill = 8,
    Singlestep = 9,
    Getregs = 12,
    Setregs = 13,
    Attach = 16,
    Detach = 17,
    Syscall = 24,
    Sysemu = 31,
    SysemuSinglestep = 32,
    Setoptions = 0x4200,
    Geteventmsg = 0x4201,
    Getsiginfo = 0x4202,
    Setsiginfo = 0x4203,
    Getregset = 0x4204,
    Setregset = 0x4205,
    Seize = 0x4206,
    Interrupt = 0x4207,
    Listen = 0x4208,
    Getsigmask = 0x420a,
    Setsigmask = 0x420b,
    Getsyscallinfo = 0x420e,
}

impl TryFrom<usize> for PtraceRequest {
    type Error = SystemError;
    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Traceme),
            1 => Ok(Self::Peektext),
            2 => Ok(Self::Peekdata),
            3 => Ok(Self::Peekuser),
            4 => Ok(Self::Poketext),
            5 => Ok(Self::Pokedata),
            6 => Ok(Self::Pokeuser),
            7 => Ok(Self::Cont),
            8 => Ok(Self::Kill),
            9 => Ok(Self::Singlestep),
            12 => Ok(Self::Getregs),
            13 => Ok(Self::Setregs),
            16 => Ok(Self::Attach),
            17 => Ok(Self::Detach),
            24 => Ok(Self::Syscall),
            31 => Ok(Self::Sysemu),
            32 => Ok(Self::SysemuSinglestep),
            0x4200 => Ok(Self::Setoptions),
            0x4201 => Ok(Self::Geteventmsg),
            0x4202 => Ok(Self::Getsiginfo),
            0x4203 => Ok(Self::Setsiginfo),
            0x4204 => Ok(Self::Getregset),
            0x4205 => Ok(Self::Setregset),
            0x4206 => Ok(Self::Seize),
            0x4207 => Ok(Self::Interrupt),
            0x4208 => Ok(Self::Listen),
            0x420a => Ok(Self::Getsigmask),
            0x420b => Ok(Self::Setsigmask),
            0x420e => Ok(Self::Getsyscallinfo),
            _ => Err(SystemError::EINVAL),
        }
    }
}

/// ptrace 事件类型（PTRACE_EVENT_*）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PtraceEvent {
    Fork = 1,
    VFork = 2,
    Clone = 3,
    Exec = 4,
    VForkDone = 5,
    Exit = 6,
    Seccomp = 7,
    /// PTRACE_EVENT_STOP（128）：seize/INTERRUPT/group-stop 在 seized 模式下产生。
    Stop = 128,
}

/// `PTRACE_GET_SYSCALL_INFO` 的 syscall 消息标识。
pub const PTRACE_EVENTMSG_SYSCALL_ENTRY: usize = 1;
pub const PTRACE_EVENTMSG_SYSCALL_EXIT: usize = 2;

// SIGTRAP 的 si_code 取值
// ptrace tracer（gdb）据 si_code 区分单步 / 硬件断点 / 软件断点。
pub const TRAP_BRKPT: i32 = 1; // 软件断点（int3）
pub const TRAP_TRACE: i32 = 2; // 单步（RFLAGS.TF）
pub const SI_KERNEL: i32 = 0x80;
/// DragonOS-v 暂未实现 BTS（分支追踪），保留供未来使用。
#[allow(dead_code)]
pub const TRAP_BRANCH: i32 = 3; // 分支陷阱（BTS）
pub const TRAP_HWBKPT: i32 = 4; // 硬件断点（DR0-3 命中）
/// x86 DR6 中单步位（BS）。do_debug 的 error_code（=DR6）置此位表示单步陷阱。
pub const X86_DR_BS: u64 = 1 << 14;
/// x86 DR6 中硬件断点命中位（B0-B3）。
pub const X86_DR_B_MASK: u64 = 0x0f;
/// x86 EFLAGS Trap Flag（单步）位。
pub const X86_EFLAGS_TF: u64 = 0x100;
/// x86 DR6 保留位。ptrace 对外暴露正极性 virtual_dr6，与硬件 DR6 互转时按此掩码翻转。
const DR6_RESERVED: u64 = 0xffff_0ff0;

// ptrace exit_code / si_code 编码
const EXITCODE_SIG_MASK: usize = 0x7f;
/// exit_code 中 event 编码的位移。
const EXITCODE_EVENT_SHIFT: u32 = 8;
/// syscall-stop 的 sysgood 标志位（需 PTRACE_O_TRACESYSGOOD）。
const PTRACE_SYSGOOD_BIT: usize = 0x80;

/// x86_64 的 ptrace_syscall_info.arch 字段值（AUDIT_ARCH_X86_64 = EM_X86_64|__AUDIT_ARCH_64BIT|__AUDIT_ARCH_LE）。
pub const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
/// Linux MAX_ERRNO：返回值在 [-MAX_ERRNO, -1] 视为错误。
pub const MAX_ERRNO: i64 = 4095;

bitflags::bitflags! {
    /// ptrace 选项（PTRACE_O_*）。
    #[derive(Default)]
    pub struct PtraceOptions: usize {
        const TRACESYSGOOD   = 1 << 0;
        const TRACEFORK      = 1 << 1;
        const TRACEVFORK     = 1 << 2;
        const TRACECLONE     = 1 << 3;
        const TRACEEXEC      = 1 << 4;
        const TRACEVFORKDONE = 1 << 5;
        const TRACEEXIT      = 1 << 6;
        const TRACESECCOMP   = 1 << 7;
        const EXITKILL       = 1 << 20;
        const SUSPEND_SECCOMP = 1 << 21;
    }
}

// PTRACE_GET_SYSCALL_INFO 结构
/// `ptrace_syscall_info.op` 取值。
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum PtraceSyscallInfoOp {
    #[default]
    None = 0,
    Entry = 1,
    Exit = 2,
    Seccomp = 3,
}

/// `ptrace_syscall_info.entry`。
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoEntry {
    pub nr: u64,
    pub args: [u64; 6],
}

/// `ptrace_syscall_info.exit`。
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoExit {
    pub rval: i64,
    pub is_error: u8,
}

/// `ptrace_syscall_info.seccomp`。
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoSeccomp {
    pub nr: u64,
    pub args: [u64; 6],
    pub ret_data: u32,
}

/// `ptrace_syscall_info` 的 data 联合体。
#[repr(C)]
#[derive(Clone, Copy)]
pub union PtraceSyscallInfoData {
    pub entry: PtraceSyscallInfoEntry,
    pub exit: PtraceSyscallInfoExit,
    pub seccomp: PtraceSyscallInfoSeccomp,
}

impl Default for PtraceSyscallInfoData {
    fn default() -> Self {
        // SAFETY: 联合体所有字段均为 POD 整数类型，零初始化有效。
        unsafe { core::mem::zeroed() }
    }
}

/// `struct ptrace_syscall_info`。
#[repr(C)]
#[derive(Clone)]
pub struct PtraceSyscallInfo {
    pub op: PtraceSyscallInfoOp,
    pub pad: [u8; 3],
    pub arch: u32,
    pub instruction_pointer: u64,
    pub stack_pointer: u64,
    pub data: PtraceSyscallInfoData,
}

impl Default for PtraceSyscallInfo {
    fn default() -> Self {
        // SAFETY: 同 PtraceSyscallInfoData。
        unsafe { core::mem::zeroed() }
    }
}

impl PtraceSyscallInfo {
    /// 最底层构造：填入架构无关的 op/arch/ip/sp，data 留空。
    pub fn new(arch: u32, ip: u64, sp: u64) -> Self {
        Self {
            op: PtraceSyscallInfoOp::None,
            pad: [0; 3],
            arch,
            instruction_pointer: ip,
            stack_pointer: sp,
            data: PtraceSyscallInfoData::default(),
        }
    }

    /// 标记为 Entry 并填充系统调用号与参数。
    pub fn with_entry(mut self, nr: u64, args: [u64; 6]) -> Self {
        self.op = PtraceSyscallInfoOp::Entry;
        self.data.entry = PtraceSyscallInfoEntry { nr, args };
        self
    }

    /// 标记为 Exit 并填充返回值与是否错误。
    pub fn with_exit(mut self, rval: i64, is_error: bool) -> Self {
        self.op = PtraceSyscallInfoOp::Exit;
        self.data.exit = PtraceSyscallInfoExit {
            rval,
            is_error: is_error as u8,
        };
        self
    }

    /// 标记为 Seccomp。
    pub fn with_seccomp(mut self, nr: u64, args: [u64; 6], ret_data: u32) -> Self {
        self.op = PtraceSyscallInfoOp::Seccomp;
        self.data.seccomp = PtraceSyscallInfoSeccomp { nr, args, ret_data };
        self
    }
}

/// 判断系统调用返回值是否为错误
#[inline(always)]
pub fn syscall_retval_is_error(retval: i64) -> bool {
    (-MAX_ERRNO..=-1).contains(&retval)
}

// x86_64 用户寄存器
/// x86_64 `struct user_regs_struct`，供 PTRACE_GETREGS/SETREGS/PEEKUSER/POKEUSER 使用。
#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Debug, Copy, Clone, Default)]
pub struct UserRegsStruct {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub bp: u64,
    pub bx: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub ax: u64,
    pub cx: u64,
    pub dx: u64,
    pub si: u64,
    pub di: u64,
    /// 系统调用号（DragonOS TrapFrame 的 errcode 字段在 syscall 时存 nr）。
    pub orig_ax: u64,
    pub ip: u64,
    pub cs: u64,
    pub flags: u64,
    pub sp: u64,
    pub ss: u64,
    pub fs_base: u64,
    pub gs_base: u64,
    pub ds: u64,
    pub es: u64,
    pub fs: u64,
    pub gs: u64,
}

#[cfg(target_arch = "x86_64")]
impl UserRegsStruct {
    /// 从 TrapFrame 构造（GETREGS 路径）。
    /// 注意 orig_ax 取 TrapFrame.errcode（syscall 时为系统调用号）。
    pub fn from_trap_frame(frame: &TrapFrame) -> Self {
        Self {
            r15: frame.r15,
            r14: frame.r14,
            r13: frame.r13,
            r12: frame.r12,
            bp: frame.rbp,
            bx: frame.rbx,
            r11: frame.r11,
            r10: frame.r10,
            r9: frame.r9,
            r8: frame.r8,
            ax: frame.rax,
            cx: frame.rcx,
            dx: frame.rdx,
            si: frame.rsi,
            di: frame.rdi,
            orig_ax: frame.errcode,
            ip: frame.rip,
            cs: frame.cs,
            flags: frame.rflags,
            sp: frame.rsp,
            ss: frame.ss,
            fs_base: 0,
            gs_base: 0,
            ds: frame.ds,
            es: frame.es,
            fs: 0,
            gs: 0,
        }
    }

    /// 写回 TrapFrame（SETREGS 路径）。
    /// 安全校验：
    /// - cs/ss 必须 RPL=3 且非零（防 ring-0 注入）
    /// - rflags 仅放行 FLAG_MASK 位，保留 frame 非掩码位（防清 IF 致用户态挂死）
    pub fn write_to_trap_frame(&self, frame: &mut TrapFrame) -> Result<(), SystemError> {
        // 段选择子校验
        // cs/ss: RPL=3，非零（SEGMENT_RPL_MASK=0x3, USER_RPL=3）。
        if (self.cs & 0x3) != 3 || self.cs == 0 {
            return Err(SystemError::EIO);
        }
        if (self.ss & 0x3) != 3 || self.ss == 0 {
            return Err(SystemError::EIO);
        }
        // rflags: 保留 frame 非掩码位，仅放行 FLAG_MASK 位。
        // FLAG_MASK = FLAG_MASK_32 | NT = CF|PF|AF|ZF|SF|TF|DF|RF|AC|NT = 0x00054DD5。
        const FLAG_MASK: u64 = 0x0005_4DD5;
        let new_rflags = (frame.rflags & !FLAG_MASK) | (self.flags & FLAG_MASK);

        frame.r15 = self.r15;
        frame.r14 = self.r14;
        frame.r13 = self.r13;
        frame.r12 = self.r12;
        frame.rbp = self.bp;
        frame.rbx = self.bx;
        frame.r11 = self.r11;
        frame.r10 = self.r10;
        frame.r9 = self.r9;
        frame.r8 = self.r8;
        frame.rax = self.ax;
        frame.rcx = self.cx;
        frame.rdx = self.dx;
        frame.rsi = self.si;
        frame.rdi = self.di;
        frame.errcode = self.orig_ax;
        frame.rip = self.ip;
        frame.cs = self.cs;
        frame.rflags = new_rflags;
        frame.rsp = self.sp;
        frame.ss = self.ss;
        frame.ds = self.ds;
        frame.es = self.es;
        Ok(())
    }
}

/// ELF NT_PRSTATUS note type（PTRACE_GETREGSET/SETREGSET 用）。
pub const NT_PRSTATUS: u32 = 1;

// PtraceState —— 跟踪状态机（对应 Linux task_struct 的 ptrace/jobctl 相关字段）
/// 进程被 ptrace 跟踪时的状态信息。
#[derive(Debug)]
pub struct PtraceState {
    /// 当前 ptrace-stop 的 exit_code（信号号或 `(event<<8)|SIGTRAP` 或 `SIGTRAP|0x80`）。
    pub exit_code: usize,
    /// tracer 在 resume 时注入的信号（0/INVALID 表示不注入）。
    pub injected_signal: Signal,
    /// 最近一次 ptrace-stop 的 siginfo 副本，供 GETSIGINFO/SETSIGINFO 读写。
    /// 必须在持本 `PtraceState` 锁时访问。
    pub last_siginfo: Option<SigInfo>,
    /// 事件消息，供 GETEVENTMSG 读取（fork/clone 的 child pid、syscall entry/exit 标识等）。
    pub event_message: usize,
    /// ptrace 选项（PTRACE_O_*）。
    pub options: PtraceOptions,
    /// PTRACE_LISTEN：tracee 处于 STOP trap 但不被 wait 视为 TRACED。
    pub listening: bool,
    /// 本 stop 仍可被 wait(2) 报告一次；消费后清零（除非 WNOWAT）。一次性标志。
    pub stop_report_pending: bool,
    /// 持久标志：tracee 当前是否处于 ptrace-stop
    pub in_ptrace_stop: bool,
    /// attach 到已 STOPPED 任务时挂起的 PTRACE_EVENT_STOP 信号。
    pub pending_event_stop: Option<Signal>,
    /// TIF_FORCED_TF：true 表示当前 TF 是调试器为 single-step 强制置位。
    pub forced_trap_flag: bool,
    /// 当前 ptrace-stop 的用户 TrapFrame 是否位于 syscall 栈。
    /// 调度上下文保存的 rsp 不能用来猜 TrapFrame 位置。
    pub stop_frame_on_syscall_stack: bool,
    /// 调试寄存器（DR0-DR7）的 ptrace 侧存储。
    pub debug_regs: [u64; 8],
}

impl Default for PtraceState {
    fn default() -> Self {
        Self {
            exit_code: 0,
            injected_signal: Signal::INVALID,
            last_siginfo: None,
            event_message: 0,
            options: PtraceOptions::empty(),
            listening: false,
            stop_report_pending: false,
            in_ptrace_stop: false,
            pending_event_stop: None,
            forced_trap_flag: false,
            stop_frame_on_syscall_stack: false,
            debug_regs: [0; 8],
        }
    }
}

impl PtraceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consume_stop_report(&mut self, consume: bool) -> Option<i32> {
        if self.listening || !self.stop_report_pending {
            return None;
        }
        let code = self.exit_code as i32;
        if consume {
            self.stop_report_pending = false;
        }
        Some(code)
    }
}

fn traceme_allowed(
    parent: &Arc<ProcessControlBlock>,
    child: &Arc<ProcessControlBlock>,
) -> Result<(), SystemError> {
    if is_ptraced_locked(child) {
        return Err(SystemError::EPERM);
    }
    if parent.flags().contains(ProcessFlags::EXITING) {
        return Err(SystemError::EPERM);
    }
    // 父进程须有权跟踪子进程。
    if !parent.has_permission_to_trace(child) {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

fn traceme_parent_for(
    child: &Arc<ProcessControlBlock>,
) -> Result<Arc<ProcessControlBlock>, SystemError> {
    let real_parent = child.real_parent_pcb().ok_or(SystemError::EPERM)?;
    let Some(fork_parent) = child.fork_parent_pcb() else {
        return Ok(real_parent);
    };

    if fork_parent.tgid == real_parent.tgid {
        Ok(fork_parent)
    } else {
        Ok(real_parent)
    }
}

pub fn traceme_current() -> Result<(), SystemError> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let current = ProcessManager::current_pcb();
    let tracer = traceme_parent_for(&current)?;
    traceme_allowed(&tracer, &current)?;

    let raw_pid = current.raw_pid();
    {
        let mut ptracer = current.ptracer_pcb.write_irqsave();
        if ptracer.upgrade().is_some() {
            return Err(SystemError::EPERM);
        }
        *ptracer = Arc::downgrade(&tracer);
        current.flags().insert(ProcessFlags::PTRACED);
    }

    let mut ptraced = tracer.ptraced.write_irqsave();
    if !ptraced.contains(&raw_pid) {
        ptraced.push(raw_pid);
    }

    Ok(())
}

pub fn unlink_tracee(tracee: &Arc<ProcessControlBlock>) {
    let tracer = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        let tracer = {
            let mut ptracer = tracee.ptracer_pcb.write_irqsave();
            let tracer = ptracer.upgrade();
            *ptracer = Weak::new();
            tracee.flags().remove(ProcessFlags::PTRACED);
            tracer
        };

        if let Some(tracer) = tracer.as_ref() {
            let raw_pid = tracee.raw_pid();
            tracer.ptraced.write_irqsave().retain(|pid| *pid != raw_pid);
        }
        tracer
    };

    // Linux wakes the ptrace parent before destroying the old leader in
    // de_thread().  DragonOS keeps separate per-task wait queues, so both the
    // tracer and natural parent must recheck their wait ownership after the
    // relation and index update become visible.
    if let Some(tracer) = tracer.as_ref() {
        ProcessManager::wake_wait_parent(tracer);
    }

    if let Some(real_parent) = tracee.real_parent_pcb() {
        if !tracer
            .as_ref()
            .map(|tracer| Arc::ptr_eq(tracer, &real_parent))
            .unwrap_or(false)
        {
            ProcessManager::wake_wait_parent(&real_parent);
        }
    }
}

pub(crate) struct TraceePidExchangePlan {
    left: Option<TraceePidUpdate>,
    right: Option<TraceePidUpdate>,
}

struct TraceePidUpdate {
    tracer: Arc<ProcessControlBlock>,
    index: usize,
    old_pid: RawPid,
    new_pid: RawPid,
}

/// Resolve tracer-side vector positions before entering the global process-map
/// IRQ-off critical section. `PTRACE_RELATION_LOCK` keeps these indices stable
/// until `commit_tracee_pid_exchange_locked()` applies the two O(1) writes.
pub(crate) fn prepare_tracee_pid_exchange_locked(
    left: &Arc<ProcessControlBlock>,
    right: &Arc<ProcessControlBlock>,
    left_old_pid: RawPid,
    right_old_pid: RawPid,
) -> TraceePidExchangePlan {
    let left_tracer = left.ptracer_pcb.read_irqsave().upgrade();
    let right_tracer = right.ptracer_pcb.read_irqsave().upgrade();
    let left = left_tracer.as_ref().map(|tracer| {
        let ptraced = tracer.ptraced.read_irqsave();
        let index = ptraced
            .iter()
            .position(|pid| *pid == left_old_pid)
            .expect("left tracee missing from tracer raw-PID index");
        TraceePidUpdate {
            tracer: tracer.clone(),
            index,
            old_pid: left_old_pid,
            new_pid: right_old_pid,
        }
    });
    let right = right_tracer.as_ref().map(|tracer| {
        let ptraced = tracer.ptraced.read_irqsave();
        let index = ptraced
            .iter()
            .position(|pid| *pid == right_old_pid)
            .expect("right tracee missing from tracer raw-PID index");
        TraceePidUpdate {
            tracer: tracer.clone(),
            index,
            old_pid: right_old_pid,
            new_pid: left_old_pid,
        }
    });

    TraceePidExchangePlan { left, right }
}

/// Update tracer-side raw-PID indices after the corresponding task identities
/// have been exchanged.  The caller must hold `PTRACE_RELATION_LOCK` and must
/// call `prepare_tracee_pid_exchange_locked()` before beginning the identity
/// transaction.
pub(crate) fn commit_tracee_pid_exchange_locked(plan: TraceePidExchangePlan) {
    for update in [plan.left, plan.right].into_iter().flatten() {
        let mut ptraced = update.tracer.ptraced.write_irqsave();
        let entry = ptraced
            .get_mut(update.index)
            .expect("tracee index changed during PID identity exchange");
        assert_eq!(
            *entry, update.old_pid,
            "tracee PID changed during identity exchange"
        );
        *entry = update.new_pid;
    }
}

/// 退出/销毁 tracer 时，阶段一持锁收集、阶段二锁外执行的每个 tracee 副作用快照。
struct ExitPtracePending {
    tracee: Arc<ProcessControlBlock>,
    /// 是否需向 tracee 发 SIGKILL（旧会话设了 PTRACE_O_EXITKILL）。
    exitkill: bool,
    /// tracee 是否处于 ptrace-stop。
    in_ptrace_stop: bool,
    /// tracee 是否仍有生效的 group-stop。
    group_stop_active: bool,
    /// 锁内取 Arc clone 保活到锁外的 real_parent。
    real_parent: Option<Arc<ProcessControlBlock>>,
}

/// 退出/销毁 tracer 时解除其所有 tracee 的跟踪关系。
pub fn exit_ptrace(tracer: &Arc<ProcessControlBlock>) {
    // 阶段一：持 PTRACE_RELATION_LOCK（关 IRQ 自旋锁）完成全部关系/状态变更，
    // 并为每个 tracee 收集锁外副作用所需的快照。发 SIGKILL 与唤醒会触发调度、
    // 争用其它锁，故移出关 IRQ 临界区，缩短中断关闭时长。
    let pending: Vec<ExitPtracePending> = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        let traced_pids: Vec<RawPid> = {
            let mut ptraced = tracer.ptraced.write_irqsave();
            core::mem::take(&mut *ptraced)
        };

        let mut pending = Vec::new();
        for pid in traced_pids {
            let Some(tracee) = ProcessManager::find(pid) else {
                continue;
            };
            // 仅当 tracee 确实由本 tracer 跟踪时才处理（防止竞态下被换 tracer）。
            let was_traced = {
                let mut ptracer = tracee.ptracer_pcb.write_irqsave();
                let mine = ptracer
                    .upgrade()
                    .as_ref()
                    .map(|t| Arc::ptr_eq(t, tracer))
                    .unwrap_or(false);
                if mine {
                    *ptracer = Weak::new();
                    tracee.flags().remove(ProcessFlags::PTRACED);
                }
                mine
            };
            if !was_traced {
                continue;
            }

            // 若 tracee 开启 PTRACE_O_EXITKILL，tracer 退出时向其发 SIGKILL
            // （在清空 options 前取出该标志）。
            let exitkill = tracee
                .ptrace_state
                .lock_irqsave()
                .options
                .contains(PtraceOptions::EXITKILL);
            // 清 syscall-trace/单步工作位，避免残留。
            tracee.flags().remove(
                ProcessFlags::TRACE_SYSCALL
                    | ProcessFlags::TRACE_SINGLESTEP
                    | ProcessFlags::TRACE_SYSEMU
                    | ProcessFlags::PT_SEIZED,
            );

            // 决策：tracee 是否真的在 ptrace-stop。
            let in_ptrace_stop = {
                let ps = tracee.ptrace_state.lock_irqsave();
                (ps.in_ptrace_stop || ps.listening) && tracee.sched_info().state().is_stopped()
            };
            // 无条件清单步 TF
            // 运行中的 tracee 可能经 PTRACE_SINGLESTEP/SYSEMU_SINGLESTEP 置了 TF，
            // 若 tracer 退出时不清，tracee 恢复执行后 #DB 触发 force_sig(SIGTRAP) 致死。
            tracee.disable_single_step();
            let group_stop_active = tracee.sighand().flags_contains(SignalFlags::STOP_STOPPED);

            // 清本 stop 的 ptrace 侧状态。
            {
                let mut ps = tracee.ptrace_state.lock_irqsave();
                ps.stop_report_pending = false;
                ps.in_ptrace_stop = false;
                ps.listening = false;
                ps.pending_event_stop = None;
                ps.exit_code = 0;
                // 清空 ptrace 选项，与 ptrace_unlink 对称：避免 tracee 被新 tracer
                // attach 后继承本会话遗留选项（EXITKILL 等）。
                ps.options = PtraceOptions::empty();
            }
            tracee.flags().remove(
                ProcessFlags::PTRACE_EVENT_STOP
                    | ProcessFlags::PENDING_PTRACE_STOP
                    | ProcessFlags::TRAPPING,
            );

            // real_parent 锁内取一次 Arc clone，保活到锁外使用。
            let real_parent = tracee.real_parent_pcb();

            pending.push(ExitPtracePending {
                tracee,
                exitkill,
                in_ptrace_stop,
                group_stop_active,
                real_parent,
            });
        }
        pending
        // _relation_guard 在此 drop，释放 PTRACE_RELATION_LOCK，恢复中断。
    };

    // 阶段二：脱离 PTRACE_RELATION_LOCK 后执行发 SIGKILL 与唤醒副作用。
    // 注意：phase1 清除关系后、phase2 执行前，并发 PTRACE_ATTACH 可能重新 attach 该 tracee。
    // PTRACE_RELATION_LOCK 是关 IRQ 自旋锁，不能跨 send_signal/wakeup 持有（会调度/取其它锁），
    // 故无法像 Linux tasklist_lock 那样跨整个 exit_ptrace 原子化。此处每个 tracee 处理前
    // 重新取锁验证未被 re-attach——缩小竞争窗口（虽不能完全闭合 lock-drop-recheck TOCTOU，
    // 但避免对已属新 tracer 的 tracee 发 SIGKILL/wakeup 的常见情况）。
    for p in pending {
        let ExitPtracePending {
            tracee,
            exitkill,
            in_ptrace_stop,
            group_stop_active,
            real_parent,
        } = p;
        let still_orphan = {
            let _g = super::PTRACE_RELATION_LOCK.lock_irqsave();
            !super::ptrace::is_ptraced_locked(&tracee)
        };
        if !still_orphan {
            // 已被并发 ATTACH 重新跟踪：新 tracer 拥有该 tracee，跳过本会话的副作用。
            continue;
        }
        if exitkill {
            // SIGKILL 不可阻塞/忽略，tracee 将被终止
            let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, tracee.clone(), PidType::PID);
        }

        if in_ptrace_stop {
            if group_stop_active && !exitkill {
                // group-stop 仍有效：保持 Stopped，设 CLD_STOPPED 让 real_parent
                // 的 wait 能报告（CLD_STOPPED 是一次性消费位，ptrace 会话期间可能已消费）。
                // exitkill 时跳过：tracee 即将被 SIGKILL 终止，不应再报 stop。
                tracee.sighand().flags_insert(SignalFlags::CLD_STOPPED);
            } else {
                // group-stop 不再有效，或 tracee 即将被 SIGKILL：无条件唤醒脱离 ptrace_stop。
                let _ = ProcessManager::wakeup_stop(&tracee);
            }
            // real_parent 的 wait 唤醒独立于 tracee 唤醒（存在则通知）。
            if let Some(real_parent) = real_parent {
                ProcessManager::wake_wait_parent(&real_parent);
            }
        } else if let Some(real_parent) = real_parent {
            // 非 ptrace-stop（如运行中）：唤醒等待者（parent + leader）。
            ProcessManager::wake_wait_parent(&real_parent);
        }
    }
}

pub fn tracees_of(tracer: &Arc<ProcessControlBlock>) -> Vec<RawPid> {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    tracees_of_locked(tracer)
}

fn tracees_of_locked(tracer: &Arc<ProcessControlBlock>) -> Vec<RawPid> {
    tracer.ptraced.read_irqsave().clone()
}

pub fn ptracer_of(tracee: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    // 快路径：PTRACED 位只在关系锁临界区内成对地与 ptracer 一起写入
    // 位为空时必然没有 tracer，无需获取全局锁。
    if !tracee.flags().contains(ProcessFlags::PTRACED) {
        return None;
    }
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    ptracer_of_locked(tracee)
}

pub(crate) fn ptracer_of_locked(
    tracee: &Arc<ProcessControlBlock>,
) -> Option<Arc<ProcessControlBlock>> {
    tracee.ptracer_pcb.read_irqsave().upgrade()
}

pub fn is_ptraced(tracee: &ProcessControlBlock) -> bool {
    // 快路径同 ptracer_of：未被跟踪时避免全局锁。
    if !tracee.flags().contains(ProcessFlags::PTRACED) {
        return false;
    }
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    is_ptraced_locked(tracee)
}

fn is_ptraced_locked(tracee: &ProcessControlBlock) -> bool {
    tracee.flags().contains(ProcessFlags::PTRACED)
        && tracee.ptracer_pcb.read_irqsave().upgrade().is_some()
}

pub fn is_wait_tracee_of(
    tracee: &Arc<ProcessControlBlock>,
    waiter: &Arc<ProcessControlBlock>,
    options: WaitOption,
) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let Some(tracer) = ptracer_of_locked(tracee) else {
        return false;
    };

    let same_waiter = Arc::ptr_eq(&tracer, waiter);
    let same_thread_group = !options.contains(WaitOption::WNOTHREAD) && tracer.tgid == waiter.tgid;
    if !same_waiter && !same_thread_group {
        return false;
    }

    tracees_of_locked(&tracer).contains(&tracee.raw_pid())
}

// PCB ptrace 方法 —— 关系建立/解除、attach/seize/detach、TRAPPING 同步
impl ProcessControlBlock {
    /// 是否有权限跟踪目标。简化版：
    /// 同线程组允许（自省）；否则要求 uid/gid 全匹配且 dumpable；或 CAP_SYS_PTRACE。
    pub fn has_permission_to_trace(&self, tracee: &Self) -> bool {
        // 1. 同一线程组允许访问（自省）
        if self.tgid == tracee.tgid {
            return true;
        }

        // 2. 凭证匹配 + dumpable
        let caller_cred = self.cred();
        let tracee_cred = tracee.cred();
        let uid_match = caller_cred.uid == tracee_cred.euid
            && caller_cred.uid == tracee_cred.suid
            && caller_cred.uid == tracee_cred.uid;
        let gid_match = caller_cred.gid == tracee_cred.egid
            && caller_cred.gid == tracee_cred.sgid
            && caller_cred.gid == tracee_cred.gid;
        if uid_match && gid_match && tracee.dumpable() != 0 {
            return true;
        }
        // 3. CAP_SYS_PTRACE：在目标（tracee）的 user_ns
        // 判定 capability，而非调用者自身 ns，避免子 user namespace 越权跟踪父 ns 进程。
        caller_cred.has_capability_in_ns(
            &tracee_cred.user_ns,
            crate::process::cred::CAPFlags::CAP_SYS_PTRACE,
        )
    }

    /// 建立跟踪关系（tracee 侧调用）。
    /// 调用者不必持 `PTRACE_RELATION_LOCK`函数会自行获取。
    pub fn ptrace_link(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if !tracer.has_permission_to_trace(self) {
            return Err(SystemError::EPERM);
        }
        // 建立新跟踪关系时清空 ptrace 选项，保证 re-attach 不继承上一会话的选项。
        // SEIZE 路径会在 link 之后覆盖为用户指定的选项；ATTACH 路径保持空选项。
        self.ptrace_link_locked(tracer, false)?;
        self.ptrace_state.lock_irqsave().options = PtraceOptions::empty();
        Ok(())
    }

    /// fork/clone 子进程自动继承父进程的跟踪关系
    /// 与 ptrace_link 的区别：不重新检查权限；tracer 正在退出时跳过（返回 Ok），不使 fork 失败。
    /// 显式 attach 路径仍须用 ptrace_link 做权限检查。
    pub fn ptrace_link_inherit(
        &self,
        tracer: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        self.ptrace_link_locked(tracer, true)
    }

    fn ptrace_link_locked(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        check_tracer_liveness: bool,
    ) -> Result<(), SystemError> {
        let raw_pid = self.raw_pid();
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();

        if check_tracer_liveness
            && (tracer.exit_state() != ExitState::Running
                || tracer.flags().contains(ProcessFlags::EXITING))
        {
            // tracer 已在 fork 的两次取锁之间退出：跳过链接，不 attach 到死 tracer。
            return Ok(());
        }

        {
            let mut ptracer = self.ptracer_pcb.write_irqsave();
            if ptracer.upgrade().is_some() {
                // 已经被跟踪
                return Err(SystemError::EPERM);
            }
            // 拒绝正在退出/已退出的目标
            if self.exit_state() != ExitState::Running {
                return Err(SystemError::EPERM);
            }
            *ptracer = Arc::downgrade(tracer);
            self.flags().insert(ProcessFlags::PTRACED);
        }

        let mut ptraced = tracer.ptraced.write_irqsave();
        if !ptraced.contains(&raw_pid) {
            ptraced.push(raw_pid);
        }
        Ok(())
    }

    /// 解除跟踪关系，并按 group-stop 状态恢复 tracee 执行状态。
    /// 从 ptraced 列表移除、清 syscall-trace 工作、按 group-stop 决定 TracedStopped→Stopped 或唤醒。
    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();

        // 取出 tracer 并清关系（复用 unlink_tracee 的核心，但需要后续状态决策）。
        let tracer = {
            let mut ptracer = self.ptracer_pcb.write_irqsave();
            let t = ptracer.upgrade();
            *ptracer = Weak::new();
            self.flags().remove(ProcessFlags::PTRACED);
            t
        };
        if let Some(tracer) = tracer.as_ref() {
            let raw_pid = self.raw_pid();
            tracer.ptraced.write_irqsave().retain(|p| *p != raw_pid);
        }

        // 清除 syscall-trace / 单步工作位，避免 detach 后残留。
        #[cfg(target_arch = "x86_64")]
        self.disable_single_step();

        self.flags().remove(
            ProcessFlags::TRACE_SYSCALL
                | ProcessFlags::TRACE_SINGLESTEP
                | ProcessFlags::TRACE_SYSEMU
                | ProcessFlags::PT_SEIZED,
        );
        // 决策：tracee 当前是否真的在 ptrace-stop（schedule 已完成 state=Stopped）。
        let in_ptrace_stop = {
            let ps = self.ptrace_state.lock_irqsave();
            (ps.in_ptrace_stop || ps.listening) && self.sched_info().state().is_stopped()
        };
        let group_stop_active = self.sighand().flags_contains(SignalFlags::STOP_STOPPED);

        // 清本 stop 的 ptrace 侧状态。
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.stop_report_pending = false;
            ps.in_ptrace_stop = false;
            ps.listening = false;
            ps.pending_event_stop = None;
            ps.exit_code = 0;
            ps.options = PtraceOptions::empty();
        }
        self.flags().remove(
            ProcessFlags::PTRACE_EVENT_STOP
                | ProcessFlags::PENDING_PTRACE_STOP
                | ProcessFlags::TRAPPING,
        );

        if in_ptrace_stop {
            if let Some(strong) = self.self_ref.upgrade() {
                if group_stop_active {
                    // group stop 仍有效：保持 Stopped（child 本就已 Stopped）
                } else {
                    // group stop 不再有效：唤醒 tracee。
                    let _ = ProcessManager::wakeup_stop(&strong);
                }
            }
        }

        Ok(())
    }

    /// 当前是否被跟踪。
    pub fn is_traced(&self) -> bool {
        // 快路径：PTRACED 位只在关系锁临界区内置位/清除，无需获取全局锁。
        if !self.flags().contains(ProcessFlags::PTRACED) {
            return false;
        }
        let _g = PTRACE_RELATION_LOCK.lock_irqsave();
        is_ptraced_locked(self)
    }

    /// 当前是否被指定 tracer 跟踪。
    pub fn is_traced_by(&self, tracer: &Arc<ProcessControlBlock>) -> bool {
        match self.self_ref.upgrade() {
            Some(me) => match ptracer_of(&me) {
                Some(t) => Arc::ptr_eq(&t, tracer),
                None => false,
            },
            None => false,
        }
    }

    fn ptrace_set_trapping(&self) {
        self.flags().insert(ProcessFlags::TRAPPING);
    }

    fn ptrace_clear_trapping(&self) {
        let was_trapping = self.flags().test_and_clear(ProcessFlags::TRAPPING);
        if was_trapping {
            // 唤醒 attach 等待者
            self.wait_queue
                .wakeup_all(Some(ProcessState::Blocked(true)));
        }
    }

    /// attach 端等待 tracee 完成 STOPPED→TRACED 过渡（TRAPPING 清零）。
    fn ptrace_wait_trapping_cleared(&self) {
        let _ = self.wait_queue.wait_event_killable(
            || !self.flags().contains(ProcessFlags::TRAPPING),
            None::<fn()>,
        );
    }

    /// 若 tracee 当前处于 group-stop（Stopped），将其直接转换为 ptrace-stop。
    fn ptrace_arm_attach_trap_if_stopped(&self) -> bool {
        // 临界区内仅做 state 重判 + ptrace_state 提交；notify 移出 pi_lock。
        let stop_sig = self.sighand().stop_signal();
        #[cfg(target_arch = "x86_64")]
        let on_syscall_stack;
        #[cfg(not(target_arch = "x86_64"))]
        let on_syscall_stack = false;

        {
            let _pi = self.sched_info().pi_lock_irqsave();
            if !self.sched_info().state().is_stopped() {
                return false;
            }
            self.ptrace_set_trapping();

            #[cfg(target_arch = "x86_64")]
            {
                let saved_rsp = self.arch_info_irqsave().kernel_rsp();
                let s = self.syscall_stack();
                let start = s.start_address().data();
                let end = s.stack_max_address().data();
                on_syscall_stack = (start..end).contains(&saved_rsp);
            }

            // 提交合成 ptrace-stop 标志；并补 PENDING_PTRACE_STOP 兜底：即使 pi_lock
            // 释放后 wakeup_stop 抢先唤醒 tracee，返回用户态路径（do_signal_or_restart）
            // 也必消费 PENDING 重新陷入 ptrace_event_stop，不丢 stop 报告。
            {
                let mut ps = self.ptrace_state.lock_irqsave();
                ps.in_ptrace_stop = true;
                ps.stop_report_pending = true;
                ps.exit_code = stop_sig as usize;
                ps.listening = false;
                ps.last_siginfo = None;
                ps.stop_frame_on_syscall_stack = on_syscall_stack;
            }
            self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        }
        // tracee 已 Stopped，TRAPPING 可立即清除（无需等待 tracee 重新调度）。
        self.ptrace_clear_trapping();
        // 合成 stop 不经 tracee 运行路径，必须显式通知 tracer 的 wait_queue，
        // 否则 tracer 的 waitpid 在 wait_event_interruptible 上不会被唤醒。
        if let Some(tracer) = self.ptracer_pcb() {
            tracer.wake_all_waiters();
        }
        true
    }

    pub fn ptrace_attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        let _exec_guard = self.exec_update_read();
        let is_same_thread_group = tracer.tgid == self.tgid;

        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }

        self.ptrace_link(tracer)?;
        let strong_ref = self.self_ref.upgrade().ok_or_else(|| {
            // self_ref 升级失败：进程正在销毁，回滚关系。
            let _ = self.ptrace_unlink();
            SystemError::ESRCH
        })?;

        // 非 SEIZE 的 ATTACH：
        // 若目标已处于 group-stop（Stopped），直接将其 group-stop 转为 ptrace-stop
        // 仅当目标未 Stopped 时才发 SIGSTOP 让其停止。
        if self.ptrace_arm_attach_trap_if_stopped() {
            // 目标已 group-stop：合成 ptrace-stop（last_siginfo=None），清 TRAPPING。
            self.ptrace_wait_trapping_cleared();
        } else {
            let mut info = SigInfo::new(
                Signal::SIGSTOP,
                0,
                SigCode::Kernel,
                SigType::Kill {
                    pid: RawPid(0),
                    uid: 0,
                },
            );
            if let Err(e) = Signal::SIGSTOP.send_signal_info_to_pcb(
                Some(&mut info),
                strong_ref.clone(),
                PidType::PID,
            ) {
                let _ = self.ptrace_unlink();
                return Err(e);
            }
        }

        Ok(0)
    }

    /// 处理 PTRACE_SEIZE。
    /// 不发送 SIGSTOP，设置 PT_SEIZED + 选项。
    pub fn ptrace_seize(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        options: PtraceOptions,
    ) -> Result<isize, SystemError> {
        let _exec_guard = self.exec_update_read();
        let is_same_thread_group = tracer.tgid == self.tgid;
        if !tracer.has_permission_to_trace(self)
            || self.flags().contains(ProcessFlags::KTHREAD)
            || is_same_thread_group
        {
            return Err(SystemError::EPERM);
        }
        // SUSPEND_SECCOMP 需要 CAP_SYS_ADMIN（DragonOS 暂未实现 checkpoint-restore），
        // 与 SETOPTIONS 路径一致拒绝，避免无权限用户挂起 tracee 的 seccomp 过滤。
        if options.contains(PtraceOptions::SUSPEND_SECCOMP) {
            return Err(SystemError::EPERM);
        }

        self.ptrace_link(tracer)?;
        self.flags().insert(ProcessFlags::PT_SEIZED);
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.options = options;
        }
        Ok(0)
    }

    /// 处理 PTRACE_DETACH。
    pub fn ptrace_detach(&self, signal: Option<Signal>) -> Result<isize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        if !self.is_traced_by(&current_pcb) {
            return Err(SystemError::EPERM);
        }

        // data=0 表示不注入信号
        let data_signal = match signal {
            None => Signal::INVALID,
            Some(s) => {
                if s == Signal::INVALID {
                    return Err(SystemError::EIO);
                }
                s
            }
        };
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.injected_signal = data_signal;
            ps.pending_event_stop = None;
            ps.stop_report_pending = false;
            // 不在此处清 in_ptrace_stop 让 ptrace_unlink 读到它，才能正确唤醒 tracee。
        }
        self.flags().remove(ProcessFlags::PTRACE_EVENT_STOP);

        self.ptrace_unlink()?;
        Ok(0)
    }

    /// ptrace 操作前置检查：tracee 是否由 current 跟踪且处于可操作状态。
    pub fn ptrace_check_attach(&self, request: PtraceRequest) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();
        if !self.is_traced_by(&current) {
            return Err(SystemError::ESRCH);
        }
        // KILL/INTERRUPT 允许在任意状态
        if matches!(request, PtraceRequest::Kill | PtraceRequest::Interrupt) {
            return Ok(());
        }
        // LISTEN 状态下不可操作
        // 其余请求要求 tracee 处于 ptrace-stop。
        let in_stop = {
            let ps = self.ptrace_state.lock_irqsave();
            if ps.listening {
                return Err(SystemError::ESRCH);
            }
            ps.in_ptrace_stop && self.sched_info().state().is_stopped()
        };
        if !in_stop {
            return Err(SystemError::ESRCH);
        }
        Ok(())
    }

    /// 设置 ptrace 选项（PTRACE_SETOPTIONS）。
    pub fn set_ptrace_options(&self, options: PtraceOptions) -> Result<(), SystemError> {
        if options.contains(PtraceOptions::SUSPEND_SECCOMP) {
            return Err(SystemError::EPERM);
        }
        let mut ps = self.ptrace_state.lock_irqsave();
        ps.options = options;
        Ok(())
    }

    /// 读最近一次 event message（PTRACE_GETEVENTMSG）。
    pub fn ptrace_get_event_message(&self) -> usize {
        self.ptrace_state.lock_irqsave().event_message
    }

    /// 系统调用栈访问器（ptrace 需要读 syscall 栈上的 trap frame）。
    fn syscall_stack(&self) -> crate::libs::rwlock::RwLockReadGuard<'_, KernelStack> {
        self.syscall_stack.read()
    }

    /// 检查当前 rsp 是否在 syscall 栈范围内
    /// 用于在 ptrace_stop 中动态判断 TrapFrame 在哪个栈上。
    #[cfg(target_arch = "x86_64")]
    fn current_stop_frame_on_syscall_stack(&self) -> bool {
        let current_rsp = x86::current::registers::rsp() as usize;
        let syscall_stack = self.syscall_stack();
        let start = syscall_stack.start_address().data();
        let end = syscall_stack.stack_max_address().data();
        (start..end).contains(&current_rsp)
    }

    /// 计算 kernel 栈上的 TrapFrame 指针。
    fn trap_frame_ptr_on_kernel_stack(stack: &KernelStack) -> *mut TrapFrame {
        let ptr = stack.stack_max_address().data() - size_of::<TrapFrame>();
        ptr as *mut TrapFrame
    }

    /// 计算 syscall 栈上的 TrapFrame 指针。
    /// 注意：init_syscall_stack 设 GS:0x0 = stack_max - 8（预留 8 字节），
    /// 所以 TrapFrame 实际在 stack_max - 8 - sizeof(TrapFrame)。
    #[cfg(target_arch = "x86_64")]
    fn trap_frame_ptr_on_syscall_stack(stack: &KernelStack) -> *mut TrapFrame {
        let ptr = stack.stack_max_address().data() - 8 - size_of::<TrapFrame>();
        ptr as *mut TrapFrame
    }

    /// 返回 tracee 当前 stop 对应的用户态 TrapFrame 指针。
    /// 根据 stop_frame_on_syscall_stack 选择正确的栈和偏移。
    fn tracee_trap_frame_ptr(&self) -> *mut TrapFrame {
        let on_syscall_stack = self.ptrace_state.lock_irqsave().stop_frame_on_syscall_stack;
        self.trap_frame_ptr_for(on_syscall_stack)
    }

    /// 按给定的栈选择计算 TrapFrame 指针。
    fn trap_frame_ptr_for(&self, on_syscall_stack: bool) -> *mut TrapFrame {
        if on_syscall_stack {
            #[cfg(target_arch = "x86_64")]
            {
                let s = self.syscall_stack();
                Self::trap_frame_ptr_on_syscall_stack(&s)
            }
            #[cfg(not(target_arch = "x86_64"))]
            {
                let s = self.kernel_stack();
                Self::trap_frame_ptr_on_kernel_stack(&s)
            }
        } else {
            let s = self.kernel_stack();
            Self::trap_frame_ptr_on_kernel_stack(&s)
        }
    }

    /// 在 ptrace_state 锁内复验 tracee 仍处于 ptrace-stop（帧稳定）。
    /// 致命信号可能在 check_attach 与此处之间唤醒 tracee，届时帧已失效，
    /// 调用方应放弃写入并返回 ESRCH。
    fn trap_frame_stable_locked(&self, ps: &PtraceState) -> bool {
        ps.in_ptrace_stop && self.sched_info().state().is_stopped()
    }

    /// 读 tracee 的用户寄存器（PTRACE_GETREGS）。
    #[cfg(target_arch = "x86_64")]
    pub fn tracee_user_regs(&self) -> UserRegsStruct {
        // SAFETY: tracee 处于 ptrace-stop，TrapFrame 稳定。
        let frame = unsafe { &*self.tracee_trap_frame_ptr() };
        let mut regs = UserRegsStruct::from_trap_frame(frame);
        if self.ptrace_state.lock_irqsave().forced_trap_flag {
            regs.flags &= !X86_EFLAGS_TF;
        }
        regs
    }

    #[cfg(target_arch = "x86_64")]
    pub fn write_tracee_user_regs(&self, regs: &UserRegsStruct) -> Result<(), SystemError> {
        let mut ps = self.ptrace_state.lock_irqsave();
        if !Self::trap_frame_stable_locked(self, &ps) {
            return Err(SystemError::ESRCH);
        }
        // SAFETY: tracee 仍处于 ptrace-stop，TrapFrame 稳定。
        let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
        regs.write_to_trap_frame(frame)?;
        if frame.rflags & X86_EFLAGS_TF != 0 {
            ps.forced_trap_flag = false;
        } else if ps.forced_trap_flag {
            frame.rflags |= X86_EFLAGS_TF;
        }
        Ok(())
    }
    #[cfg(target_arch = "x86_64")]
    pub fn ptrace_peek_user(&self, offset: usize) -> Result<usize, SystemError> {
        // 8 字节对齐校验
        if offset & (size_of::<u64>() - 1) != 0 {
            return Err(SystemError::EIO);
        }
        const SIZEOF_USER: usize = 928; // sizeof(struct user) x86_64
        const DR_OFFSET: usize = 848; // offsetof(struct user, u_debugreg[0])
        const GP_REGS_SIZE: usize = size_of::<UserRegsStruct>(); // 216
        if offset
            .checked_add(size_of::<u64>())
            .is_none_or(|end| end > SIZEOF_USER)
        {
            return Err(SystemError::EIO);
        }
        // 通用寄存器区：offset 0..GP_REGS_SIZE
        if offset < GP_REGS_SIZE {
            let regs = self.tracee_user_regs();
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    &regs as *const UserRegsStruct as *const u8,
                    GP_REGS_SIZE,
                )
            };
            let mut buf = [0u8; 8];
            buf.copy_from_slice(&bytes[offset..offset + 8]);
            return Ok(u64::from_ne_bytes(buf) as usize);
        }
        // 调试寄存器区：offset DR_OFFSET..=DR_OFFSET+56 (DR0-DR7, 8个slot)
        if (DR_OFFSET..DR_OFFSET + 8 * 8).contains(&offset) {
            let idx = (offset - DR_OFFSET) / 8;
            let mut val = self.ptrace_state.lock_irqsave().debug_regs[idx];
            // DR6(slot6) 存储的是 virtual_dr6（正极性），返回时翻转回硬件极性。
            if idx == 6 {
                val ^= DR6_RESERVED;
            }
            return Ok(val as usize);
        }
        // 间隙区（GP_REGS_SIZE..DR_OFFSET 之间的填充字段）静默返回 0。
        Ok(0)
    }

    /// PTRACE_POKEUSER：按字节偏移写一个字。
    /// 要求 8 字节对齐，经 putreg 校验。
    #[cfg(target_arch = "x86_64")]
    pub fn ptrace_poke_user(&self, offset: usize, value: usize) -> Result<(), SystemError> {
        const SIZEOF_USER: usize = 928;
        const DR_OFFSET: usize = 848;
        const GP_REGS_SIZE: usize = size_of::<UserRegsStruct>();
        if offset & (size_of::<u64>() - 1) != 0 {
            return Err(SystemError::EIO);
        }
        if offset
            .checked_add(size_of::<u64>())
            .is_none_or(|end| end > SIZEOF_USER)
        {
            return Err(SystemError::EIO);
        }
        let val = value as u64;
        // 通用寄存器区：经 putreg 校验后写回 trap frame。
        if offset < GP_REGS_SIZE {
            let mut regs = self.tracee_user_regs();
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    &mut regs as *mut UserRegsStruct as *mut u8,
                    GP_REGS_SIZE,
                )
            };
            bytes[offset..offset + 8].copy_from_slice(&val.to_ne_bytes());
            self.write_tracee_user_regs(&regs)?;
            return Ok(());
        }
        // 调试寄存器区。
        if (DR_OFFSET..DR_OFFSET + 8 * 8).contains(&offset) {
            let idx = (offset - DR_OFFSET) / 8;
            // DR4/DR5 不存在，拒绝写入。
            if idx == 4 || idx == 5 {
                return Err(SystemError::EIO);
            }
            let mut ps = self.ptrace_state.lock_irqsave();
            // DR6(slot6) 存储正极性 virtual_dr6，写入时翻转。
            let stored = if idx == 6 { val ^ DR6_RESERVED } else { val };
            ps.debug_regs[idx] = stored;
            return Ok(());
        }
        // 间隙区拒绝写入。
        Err(SystemError::EIO)
    }

    /// PTRACE_GETSIGINFO：读 last_siginfo。
    pub fn ptrace_get_siginfo(&self) -> Result<SigInfo, SystemError> {
        let ps = self.ptrace_state.lock_irqsave();
        ps.last_siginfo.ok_or(SystemError::EINVAL)
    }

    /// PTRACE_SETSIGINFO：写 last_siginfo。
    pub fn ptrace_set_siginfo(&self, info: SigInfo) -> Result<(), SystemError> {
        let mut ps = self.ptrace_state.lock_irqsave();
        if ps.last_siginfo.is_none() {
            return Err(SystemError::EINVAL);
        }
        ps.last_siginfo = Some(info);
        Ok(())
    }

    /// PTRACE_GETSIGMASK：读当前阻塞掩码。
    /// 返回 SigSet（DragonOS-v GenericSigSet，u64）。
    pub fn ptrace_get_sigmask(&self) -> SigSet {
        let g = self.sig_info_irqsave();
        *g.sig_blocked()
    }

    /// PTRACE_SETSIGMASK：设置阻塞掩码（SIGKILL/SIGSTOP 不可阻塞）。
    pub fn ptrace_set_sigmask(&self, mut new_set: SigSet) {
        new_set.remove(SigSet::SIGKILL);
        new_set.remove(SigSet::SIGSTOP);
        let mut g = self.sig_info_mut();
        *g.sig_block_mut() = new_set;
    }

    // PEEKDATA / POKEDATA —— 通过页表翻译访问 tracee 用户内存

    /// PTRACE_PEEKDATA/PEEKTEXT：读 tracee 用户空间一个 word。
    /// 通过 tracee 的 AddressSpace 页表翻译虚拟地址→物理地址→内核可访问虚拟地址。
    /// 正确处理跨页 word（addr 位于页尾时 8 字节跨两页）。
    pub fn ptrace_peek_data(&self, addr: usize) -> Result<usize, SystemError> {
        // 防回绕校验
        let last = addr
            .checked_add(size_of::<usize>() - 1)
            .ok_or(SystemError::EIO)?;
        if last >= MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EIO);
        }
        let mut bytes = [0u8; size_of::<usize>()];
        // 整 word 在单次 AddressSpace 读锁内拷
        let n = Self::access_user_chunk_read(self, addr, &mut bytes)?;
        if n != size_of::<usize>() {
            return Err(SystemError::EIO);
        }
        Ok(usize::from_ne_bytes(bytes))
    }

    /// PTRACE_POKEDATA/POKETEXT：写 tracee 用户空间一个 word。
    pub fn ptrace_poke_data(&self, addr: usize, value: usize) -> Result<(), SystemError> {
        let last = addr
            .checked_add(size_of::<usize>() - 1)
            .ok_or(SystemError::EIO)?;
        if last >= MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EIO);
        }
        let bytes = value.to_ne_bytes();
        let n = Self::access_user_chunk_write(self, addr, &bytes)?;
        if n != size_of::<usize>() {
            return Err(SystemError::EIO);
        }
        Ok(())
    }

    /// 读 tracee 用户空间一段连续字节（用目标进程当前地址空间）。
    /// 返回实际读取字节数；对未映射/不可访问页按 Linux mem_rw 短读语义终止。
    pub(crate) fn access_user_chunk_read(
        &self,
        addr: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let target_vm = self.basic().user_vm().clone().ok_or(SystemError::ESRCH)?;
        Self::access_remote_chunk(&target_vm, addr, Some(buf), None)
    }

    /// 写 tracee 用户空间一段连续字节（用目标进程当前地址空间）。
    /// 形参为不可变切片，避免调用方为凑可变形参而做无谓拷贝。
    /// 返回实际写入字节数；对未映射/不可访问页按 Linux mem_rw 短写语义终止。
    pub(crate) fn access_user_chunk_write(
        &self,
        addr: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let target_vm = self.basic().user_vm().clone().ok_or(SystemError::ESRCH)?;
        Self::access_remote_chunk(&target_vm, addr, None, Some(buf))
    }

    /// 在指定目标地址空间上读一段连续字节。
    /// 供 /proc/[pid]/mem 使用：target_vm 来自打开时钉住的 AddressSpace，
    /// 而非目标进程的 live mm，避免目标 execve 后访问到新地址空间。
    pub(crate) fn access_user_chunk_on_vm_read(
        target_vm: &Arc<ucontext::AddressSpace>,
        addr: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        Self::access_remote_chunk(target_vm, addr, Some(buf), None)
    }

    /// 在指定目标地址空间上写一段连续字节（不可变形参，避免调用方拷贝）。
    pub(crate) fn access_user_chunk_on_vm_write(
        target_vm: &Arc<ucontext::AddressSpace>,
        addr: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        Self::access_remote_chunk(target_vm, addr, None, Some(buf))
    }

    /// 跨进程批量读写一段连续字节的核心实现。
    fn access_remote_chunk(
        target_vm: &Arc<ucontext::AddressSpace>,
        addr: usize,
        mut read_buf: Option<&mut [u8]>,
        write_buf: Option<&[u8]>,
    ) -> Result<usize, SystemError> {
        let total = match (
            read_buf.as_ref().map(|b| b.len()),
            write_buf.as_ref().map(|b| b.len()),
        ) {
            (Some(n), _) | (_, Some(n)) => n,
            (None, None) => return Ok(0),
        };
        let write = write_buf.is_some();
        if total == 0 {
            return Ok(0);
        }

        let mut copied = 0usize;
        // 每地址最多 fault-in 一次：记录上一次 fault-in 的地址，避免死循环。
        let mut faulted_at: Option<usize> = None;

        while copied < total {
            let cur = match addr.checked_add(copied) {
                Some(c) => c,
                None => return Ok(copied),
            };
            if cur >= MMArch::USER_END_VADDR.data() {
                return Ok(copied);
            }

            // 读锁内：连续拷尽可能多的已映射页（跨页不重取锁）。
            let advanced = {
                let target_guard = target_vm.read();
                let mut local = 0usize;
                while copied + local < total {
                    let p = match addr.checked_add(copied + local) {
                        Some(v) => v,
                        None => break,
                    };
                    if p >= MMArch::USER_END_VADDR.data() {
                        break;
                    }
                    let page_off = p & (MMArch::PAGE_SIZE - 1);
                    // 本页内可拷字节：每页一次钳位（对齐 access_remote_vm）。
                    let chunk =
                        core::cmp::min(total - (copied + local), MMArch::PAGE_SIZE - page_off);
                    let pvaddr = VirtAddr::new(p);
                    // 每页一次 VMA 校验 + 一次页表 translate。
                    let Some(_vma) = target_guard.mappings.contains(pvaddr) else {
                        break;
                    };
                    let Some((phys_frame, entry_flags)) =
                        target_guard.user_mapper.utable.translate(pvaddr)
                    else {
                        break;
                    };
                    if write && !entry_flags.has_write() {
                        // 只读页：写需 COW，落到读锁外 fault-in。
                        break;
                    }
                    let Some(kernel_base) = (unsafe {
                        MMArch::phys_2_virt(PhysAddr::new(phys_frame.data() + page_off))
                    }) else {
                        break;
                    };
                    let kvptr = kernel_base.data() as *mut u8;
                    unsafe {
                        match (read_buf.as_deref_mut(), write_buf) {
                            (Some(rb), _) => core::ptr::copy_nonoverlapping(
                                kvptr as *const u8,
                                rb[copied + local..copied + local + chunk].as_mut_ptr(),
                                chunk,
                            ),
                            (_, Some(wb)) => core::ptr::copy_nonoverlapping(
                                wb[copied + local..copied + local + chunk].as_ptr(),
                                kvptr,
                                chunk,
                            ),
                            // 不会到达：read_buf / write_buf 恰一为 Some。
                            _ => {}
                        }
                    }
                    local += chunk;
                    // 跨页后不 break：同一读锁内继续处理下一页。
                }
                local
            }; // 读锁 drop

            if advanced > 0 {
                copied += advanced;
                faulted_at = None; // 有进展，重置 fault 记录。
                continue;
            }

            // 当前页未映射/需 COW：drop 读锁后（上面已 drop）锁外 fault-in 再重试。
            if faulted_at == Some(cur) {
                // 本地址已 fault-in 过仍无推进：按 short-read 终止。
                return if copied > 0 {
                    Ok(copied)
                } else {
                    Err(SystemError::EIO)
                };
            }
            match Self::fault_in_tracee_page(target_vm, VirtAddr::new(cur), write) {
                Ok(()) => {
                    faulted_at = Some(cur);
                    continue; // 重新取读锁重试本页。
                }
                Err(_) => {
                    return if copied > 0 {
                        Ok(copied)
                    } else {
                        Err(SystemError::EIO)
                    };
                }
            }
        }
        Ok(copied)
    }

    /// 在 tracee 地址空间中 fault-in 一个页。
    /// 对 file-backed VMA 先 prefault page_cache，再 handle_mm_fault（设 FAULT_FLAG_REMOTE）。
    fn fault_in_tracee_page(
        target_vm: &Arc<ucontext::AddressSpace>,
        address: VirtAddr,
        write: bool,
    ) -> Result<(), SystemError> {
        let mut flags = fault::FaultFlags::FAULT_FLAG_REMOTE;
        if write {
            flags |= fault::FaultFlags::FAULT_FLAG_WRITE;
        }

        // 对 file-backed VMA 预建 page_cache，确保后续 handle_mm_fault 能命中。
        let file_backed_vma = {
            let space_guard = target_vm.read();
            let vma = match space_guard.mappings.find_nearest(address) {
                Some(vma) => vma,
                None => return Err(SystemError::EIO),
            };
            let vma_guard = vma.lock();
            if vma_guard.region().contains(address) && vma_guard.vm_file().is_some() {
                Some(vma.clone())
            } else {
                None
            }
        };

        if let Some(vma) = file_backed_vma {
            Self::prefault_file_backing(&vma, address)?;
        }

        // handle_mm_fault
        let mut space_guard = target_vm.write();
        let vma = match space_guard.mappings.find_nearest(address) {
            Some(vma) => vma,
            None => return Err(SystemError::EIO),
        };

        let vma_guard = vma.lock();
        let region = *vma_guard.region();
        let vm_flags = *vma_guard.vm_flags();
        drop(vma_guard);

        if !region.contains(address) {
            if !vm_flags.contains(VmFlags::VM_GROWSDOWN) {
                return Err(SystemError::EIO);
            }
            let extension_size = region.start().data() - address.data();
            let max_stack_limit = space_guard
                .user_stack
                .as_ref()
                .map(|s| s.max_limit())
                .unwrap_or(0);
            if extension_size > max_stack_limit || !space_guard.can_extend_stack(extension_size) {
                return Err(SystemError::EIO);
            }
            space_guard
                .extend_stack(extension_size)
                .map_err(|_| SystemError::EIO)?;
        }

        let fault = unsafe {
            let mm = space_guard.outer_addr_space().ok_or(SystemError::EFAULT)?;
            let mapper = &mut space_guard.user_mapper.utable;
            fault::PageFaultHandler::handle_mm_fault(fault::PageFaultMessage::new(
                vma, address, flags, mapper, mm,
            ))
        };

        if fault.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
            Ok(())
        } else {
            Err(SystemError::EIO)
        }
    }

    /// 预建 file-backed VMA 的 page_cache 页。
    fn prefault_file_backing(
        vma: &Arc<ucontext::LockedVMA>,
        address: VirtAddr,
    ) -> Result<(), SystemError> {
        let (file, base_pgoff, region_start) = {
            let vma_guard = vma.lock();
            let file = vma_guard.vm_file().ok_or(SystemError::EIO)?;
            let base_pgoff = vma_guard.backing_page_offset().ok_or(SystemError::EIO)?;
            (file, base_pgoff, vma_guard.region().start().data())
        };

        let page_index = base_pgoff + ((address.data() - region_start) >> MMArch::PAGE_SHIFT);
        let inode = file.inode();
        let file_size = inode.metadata()?.size.max(0) as usize;
        if file_size == 0 || page_index.saturating_mul(MMArch::PAGE_SIZE) >= file_size {
            return Err(SystemError::EIO);
        }

        let page_cache = inode.page_cache().ok_or(SystemError::EIO)?;
        let _ = page_cache.manager().commit_page(page_index)?;
        Ok(())
    }

    /// PTRACE_GET_SYSCALL_INFO。
    /// 根据 last_siginfo 的 si_code（SIGTRAP|0x80 syscall，或 SIGTRAP|(SECCOMP<<8)）
    /// 和 event_message（ENTRY/EXIT）决定 op，读 trap frame 填 nr/args/ip/sp。
    #[cfg(target_arch = "x86_64")]
    pub fn ptrace_get_syscall_info(
        &self,
        _user_size: usize,
    ) -> Result<PtraceSyscallInfo, SystemError> {
        let (op, ret_data) = {
            let ps = self.ptrace_state.lock_irqsave();
            let code = ps
                .last_siginfo
                .as_ref()
                .map(|i| i.sig_code().as_i32())
                .unwrap_or(0);
            let msg = ps.event_message;
            let op = match (code, msg) {
                (c, PTRACE_EVENTMSG_SYSCALL_ENTRY)
                    if (c & 0xff) == (Signal::SIGTRAP as i32 | PTRACE_SYSGOOD_BIT as i32) =>
                {
                    PtraceSyscallInfoOp::Entry
                }
                (c, PTRACE_EVENTMSG_SYSCALL_EXIT)
                    if (c & 0xff) == (Signal::SIGTRAP as i32 | PTRACE_SYSGOOD_BIT as i32) =>
                {
                    PtraceSyscallInfoOp::Exit
                }
                _ if (code >> 8) == PtraceEvent::Seccomp as i32 => PtraceSyscallInfoOp::Seccomp,
                _ => PtraceSyscallInfoOp::None,
            };
            // Seccomp 停止时 ret_data = SECCOMP_RET_DATA
            let ret_data = if op == PtraceSyscallInfoOp::Seccomp {
                msg as u32
            } else {
                0
            };
            (op, ret_data)
        };
        // 读 trap frame 填 ip/sp/nr/args。
        let frame = unsafe { &*self.tracee_trap_frame_ptr() };
        // AUDIT_ARCH_X86_64（x86_64 唯一支持架构；多架构时改为 arch 提供）。
        let arch: u32 = AUDIT_ARCH_X86_64;
        let mut info = PtraceSyscallInfo::new(arch, frame.rip, frame.rsp);
        match op {
            PtraceSyscallInfoOp::Entry | PtraceSyscallInfoOp::Seccomp => {
                let nr = frame.errcode;
                let args = [
                    frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9,
                ];
                info = info.with_entry(nr, args);
                if op == PtraceSyscallInfoOp::Seccomp {
                    info = info.with_seccomp(nr, args, ret_data);
                }
            }
            PtraceSyscallInfoOp::Exit => {
                let rval = frame.rax as i64;
                let is_error = syscall_retval_is_error(rval);
                info = info.with_exit(rval, is_error);
            }
            PtraceSyscallInfoOp::None => {}
        }
        Ok(info)
    }

    // 核心停止状态机

    /// 进入 ptrace-stop
    pub fn ptrace_stop(
        &self,
        exit_code: usize,
        why: SigChildCode,
        info: Option<&mut SigInfo>,
    ) -> usize {
        // 1. 关中断（schedule 前才释放，保证检查与 commit 原子）。
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 2. 关系检查 + arm TRAPPING + commit Stopped 必须在同一 PTRACE_RELATION_LOCK 临界区内，关闭 detach race
        let relation_guard = super::PTRACE_RELATION_LOCK.lock_irqsave();
        if !super::ptrace::is_ptraced_locked(self) {
            drop(relation_guard);
            drop(irq);
            self.ptrace_clear_trapping();
            return exit_code;
        }

        // 3. arm TRAPPING（若 attach 已 set）。
        self.ptrace_set_trapping();

        // 4. fatal 检查 + commit TRACED 状态。
        let abort = {
            let sighand = self.sighand();
            let sighand_g = sighand.inner_read();
            let siginfo_g = self.sig_info_irqsave();
            let fatal = siginfo_g
                .sig_pending()
                .signal()
                .contains(Signal::SIGKILL.into())
                || sighand_g
                    .shared_pending
                    .signal()
                    .contains(Signal::SIGKILL.into());
            if fatal {
                true
            } else {
                let mut ps = self.ptrace_state.lock_irqsave();
                ps.exit_code = exit_code;
                ps.listening = false;
                ps.stop_report_pending = true;
                ps.in_ptrace_stop = true;
                #[cfg(target_arch = "x86_64")]
                {
                    ps.stop_frame_on_syscall_stack = self.current_stop_frame_on_syscall_stack();
                }
                match info.as_ref() {
                    Some(i) => ps.last_siginfo = Some(**i),
                    None => ps.last_siginfo = None,
                }
                drop(ps);
                self.sched_info().set_state(ProcessState::Stopped);
                false
            }
            // siginfo_g 与 sighand_g 在 set_state 之后才 drop
        };

        drop(relation_guard);

        // 5. fence(Release)：保证 Stopped + exit_code 在 TRAPPING 清除前对 tracer 可见。
        fence(Ordering::Release);

        if abort {
            self.ptrace_clear_trapping();
            return 0;
        }

        // 6. 清 TRAPPING，唤醒 attach 等待者。
        self.ptrace_clear_trapping();

        // group-stop 参与记账 + real_parent CLD_STOPPED 通知。
        let gstop_done = if why == SigChildCode::Stopped {
            let stop_sig = Signal::from((exit_code & EXITCODE_SIG_MASK) as i32);
            if crate::ipc::signal_types::SIG_KERNEL_STOP_MASK.contains(stop_sig.into()) {
                self.sighand().ptrace_participate_group_stop(stop_sig)
            } else {
                false
            }
        } else {
            false
        };
        // 7. 通知 tracer 并阻塞。
        if let Some(tracer) = self.ptracer_pcb() {
            self.notify_tracer(&tracer, why, exit_code);
        }
        // real_parent 的 CLD_STOPPED 通知：仅 group-stop 完成 && ptracer≠real_parent。
        if gstop_done {
            let real_parent_to_notify = match (self.ptracer_pcb(), self.real_parent_pcb()) {
                (Some(ptracer), Some(rp)) if ptracer.tgid != rp.tgid => Some(rp),
                (None, Some(rp)) => Some(rp),
                _ => None,
            };
            if let Some(rp) = real_parent_to_notify {
                let status = (exit_code & EXITCODE_SIG_MASK) as i32;
                let mut chld = SigInfo::new(
                    Signal::SIGCHLD,
                    0,
                    SigCode::Raw(SigChildCode::Stopped as i32),
                    SigType::SigChild {
                        pid: self.raw_pid(),
                        uid: 0,
                        status,
                        utime: 0,
                        stime: 0,
                    },
                );
                // real_parent 设了 SA_NOCLDSTOP 或 SIG_IGN 时不发 CLD_STOPPED
                let send = match rp.sighand().handler(Signal::SIGCHLD) {
                    Some(a) => {
                        !a.action().is_ignore() && !a.flags().contains(SigFlags::SA_NOCLDSTOP)
                    }
                    None => true,
                };
                if send {
                    let _ = Signal::SIGCHLD.send_signal_info_to_pcb(
                        Some(&mut chld),
                        rp.clone(),
                        PidType::TGID,
                    );
                }
                rp.wake_all_waiters();
            }
        }
        schedule(SchedMode::SM_NONE);
        // 8. 唤醒后清理。
        let mut ps = self.ptrace_state.lock_irqsave();
        if let Some(i) = info {
            if let Some(saved) = ps.last_siginfo {
                // 回填：PTRACE_SETSIGINFO 的修改参与后续信号递送。
                *i = saved;
            }
        }
        let injected = ps.injected_signal;
        ps.listening = false;
        ps.stop_report_pending = false;
        ps.in_ptrace_stop = false;
        ps.last_siginfo = None;
        ps.event_message = 0;
        ps.exit_code = 0;
        let result = if injected == Signal::INVALID {
            0
        } else {
            ps.injected_signal = Signal::INVALID;
            injected as usize
        };
        drop(ps);

        // 唤醒后重算信号 pending（可能被 tracer 注入了信号）。
        if let Some(strong) = self.self_ref.upgrade() {
            strong.recalc_sigpending();
        }

        result
    }

    /// 发送 SIGCHLD + 唤醒 tracer wait_queue。
    fn notify_tracer(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        why: SigChildCode,
        stop_code: usize,
    ) {
        // 1. 发送 SIGCHLD 给 tracer（如果 tracer 不忽略）。
        let should_send = {
            let sa = tracer.sighand().handler(Signal::SIGCHLD);
            let force = why == SigChildCode::Trapped;
            match sa {
                Some(a) => {
                    !a.action().is_ignore()
                        && (force || !a.flags().contains(SigFlags::SA_NOCLDSTOP))
                }
                None => false,
            }
        };
        if should_send {
            let status = match why {
                SigChildCode::Stopped | SigChildCode::Trapped => {
                    (stop_code & EXITCODE_SIG_MASK) as i32
                }
                _ => Signal::SIGCONT as i32,
            };
            let mut chld = SigInfo::new(
                Signal::SIGCHLD,
                0,
                SigCode::Raw(why as i32),
                SigType::SigChild {
                    pid: self.raw_pid(),
                    uid: 0,
                    status,
                    utime: 0,
                    stime: 0,
                },
            );
            let _ = Signal::SIGCHLD.send_signal_info_to_pcb(
                Some(&mut chld),
                tracer.clone(),
                PidType::TGID,
            );
        }
        // 无条件唤醒 ptracer 的 wait_queue。
        // gdb/strace 默认不装 SIGCHLD handler，靠 waitpid(2) 阻塞
        // 此唤醒是它们能观察到 ptrace-stop 的唯一可靠路径（上面的 SIGCHLD 仅服务信号驱动型 tracer）。
        tracer.wake_all_waiters();
        // group leader 与 ptracer 不同时也唤醒
        let leader = tracer
            .thread
            .read_irqsave()
            .group_leader()
            .unwrap_or_else(|| tracer.clone());
        if !Arc::ptr_eq(&leader, tracer) {
            leader.wake_all_waiters();
        }
    }

    /// ptrace 事件通知（FORK/CLONE/VFORK/EXEC/EXIT/SECCOMP）。
    pub fn ptrace_event(&self, event: PtraceEvent, message: usize) {
        if self.ptrace_event_enabled(event) {
            self.ptrace_state.lock_irqsave().event_message = message;
            let exit_code = (event as usize) << EXITCODE_EVENT_SHIFT | Signal::SIGTRAP as usize;
            // 仅 signal-delivery-stop 与 syscall-stop 消费 ptrace_notify 返回的注入信号；
            // FORK/CLONE/VFORK/EXEC/EXIT/SECCOMP 事件不应向 tracee 重注入 tracer 经 CONT 传入的信号。
            let _ = Self::ptrace_notify(exit_code, exit_code as i32);
        }
    }

    /// 检查事件选项是否开启。
    pub fn ptrace_event_enabled(&self, event: PtraceEvent) -> bool {
        let flag = match event {
            PtraceEvent::Fork => PtraceOptions::TRACEFORK,
            PtraceEvent::VFork => PtraceOptions::TRACEVFORK,
            PtraceEvent::Clone => PtraceOptions::TRACECLONE,
            PtraceEvent::Exec => PtraceOptions::TRACEEXEC,
            PtraceEvent::VForkDone => PtraceOptions::TRACEVFORKDONE,
            PtraceEvent::Exit => PtraceOptions::TRACEEXIT,
            PtraceEvent::Seccomp => PtraceOptions::TRACESECCOMP,
            _ => return false,
        };
        self.ptrace_state.lock_irqsave().options.contains(flag)
    }

    /// 构造 PTRACE_EVENT_STOP（seize 模式 group-stop / INTERRUPT / LISTEN 重陷）。
    pub(crate) fn ptrace_event_stop(&self, signal: Signal) -> usize {
        let exit_code = (PtraceEvent::Stop as usize) << EXITCODE_EVENT_SHIFT | signal as usize;
        // si_code 用 exit_code，GETSIGINFO 读到 (Stop<<8)|signal。
        let mut info = SigInfo::new(
            signal,
            0,
            SigCode::Raw(exit_code as i32),
            SigType::Kill {
                pid: RawPid(0),
                uid: 0,
            },
        );
        self.ptrace_stop(exit_code, SigChildCode::Stopped, Some(&mut info))
    }

    /// SIGCONT 投递路径调用，让 seized tracee 离开 group-stop/LISTEN 重新陷入 PTRACE_EVENT_STOP。
    pub fn ptrace_trap_notify(&self) {
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return;
        }
        // 单次持锁原子地写 pending_event_stop 并读唤醒门控：
        // 仅 LISTEN 或 group-stop（非 ptrace-stop）态需 wakeup_stop 重陷；
        // 正常 ptrace-stop 不打扰，PENDING 留待 CONT 后在 do_signal_or_restart 重检。
        let do_wake = {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.pending_event_stop = Some(Signal::SIGTRAP);
            ps.listening || !ps.in_ptrace_stop
        };
        self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        if do_wake {
            if let Some(strong) = self.self_ref.upgrade() {
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
    }

    /// PTRACE_INTERRUPT：让运行中的 SEIZED tracee 进入 ptrace-stop。
    pub fn ptrace_interrupt(&self) -> Result<(), SystemError> {
        // 要求 PT_SEIZED
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return Err(SystemError::EIO);
        }
        // 单次持锁原子地写 pending_event_stop 并读唤醒门控，避免多次取锁间的 TOCTOU。
        // 仅 LISTEN 或 group-stop（非 ptrace-stop）态的 Stopped tracee 需 wakeup_stop 重陷；
        // 正常 ptrace-stop 不打扰，PENDING 留待 CONT 后在 do_signal_or_restart 重检。
        let wake_for_retrap = {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.pending_event_stop = Some(Signal::SIGTRAP);
            ps.listening || !ps.in_ptrace_stop
        };
        self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        if let Some(strong) = self.self_ref.upgrade() {
            // wakeup_stop 对非 Stopped 进程是 no-op（幂等）；wakeup 对 Stopped 是 no-op。
            // 故 stale 的 state 读至多一次冗余 kick，粘性 PENDING_PTRACE_STOP 保证最终必停。
            if strong.sched_info().state().is_stopped() && wake_for_retrap {
                let _ = ProcessManager::wakeup_stop(&strong);
            } else {
                if strong.sched_info().state().is_blocked_interruptable() {
                    let _ = ProcessManager::wakeup(&strong);
                }
                ProcessManager::kick(&strong);
            }
        }
        Ok(())
    }

    /// PTRACE_LISTEN：让处于 PTRACE_EVENT_STOP 的 tracee 脱离 ptrace-stop 但保持 stopped。
    /// 语义：tracee 既不运行（保持 Stopped）也不在 ptrace-stop（wait 不可见、ptrace 命令失败 ESRCH）；
    /// group-stop 来源的 LISTEN 下信号排队但不投递，SIGCONT 使其离开 group-stop 重陷。
    pub fn ptrace_listen(&self) -> Result<(), SystemError> {
        // PT_SEIZED
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return Err(SystemError::EIO);
        }
        let mut ps = self.ptrace_state.lock_irqsave();
        // 仅在 PTRACE_EVENT_STOP trap 允许 LISTEN
        let is_event_stop = ps
            .last_siginfo
            .as_ref()
            .map(|i| (i.sig_code().as_i32() >> 8) == PtraceEvent::Stop as i32)
            .unwrap_or(false);
        if !ps.in_ptrace_stop || !is_event_stop {
            return Err(SystemError::EIO);
        }
        // 取出触发本次 event-stop 的信号（低字节）。
        let stop_signal = Signal::from(ps.exit_code as i32 & EXITCODE_SIG_MASK as i32);
        let retrap = ps.pending_event_stop.is_some();
        ps.listening = true;
        // 清 in_ptrace_stop：LISTEN 状态下 tracee 不在 ptrace-stop，
        // ptrace_check_attach 应返回 ESRCH
        ps.in_ptrace_stop = false;
        // 关键：清 stop_report_pending，使 wait 不再返回此 stop
        ps.stop_report_pending = false;
        drop(ps);
        // group-stop 来源（SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU）：置 STOP_STOPPED，
        // 使 tracee 保持 group-stop 语义（信号排队不投递；SIGCONT 经 ptrace_trap_notify 重陷）。
        if stop_signal != Signal::INVALID
            && crate::ipc::signal_types::SIG_KERNEL_STOP_MASK.contains(stop_signal.into())
        {
            self.sighand().set_stop_signal(stop_signal);
            self.sighand().flags_insert(SignalFlags::STOP_STOPPED);
        }
        if retrap {
            // 唤醒 tracee 脱离 ptrace_stop 的 schedule，使其经返回用户态路径
            // 消费 PENDING_PTRACE_STOP 并重新陷入 PTRACE_EVENT_STOP。
            if let Some(strong) = self.self_ref.upgrade() {
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
        Ok(())
    }

    /// 发送一个 ptrace 通知 stop（exit_code 必须是 SIGTRAP 编码）。
    /// `si_code` 写入 siginfo（如 TRAP_BRKPT/TRAP_TRACE/TRAP_HWBKPT，或 EVENT_STOP 编码）。
    pub fn ptrace_notify(exit_code: usize, si_code: i32) -> Result<usize, SystemError> {
        let current = ProcessManager::current_pcb();
        if (exit_code & (0x7f | !0xffff)) != Signal::SIGTRAP as usize {
            return Err(SystemError::EINVAL);
        }
        let mut info = SigInfo::new(
            Signal::SIGTRAP,
            0,
            SigCode::Raw(si_code),
            SigType::Kill {
                pid: current.raw_pid(),
                uid: 0,
            },
        );
        let signr = current.ptrace_stop(exit_code, SigChildCode::Trapped, Some(&mut info));
        Ok(signr)
    }

    /// 重新注入 ptrace_stop 返回的信号（若非 0）。
    pub fn reinject_ptrace_signal(signr: usize) {
        if signr == 0 {
            return;
        }
        let sig = Signal::from(signr as i32);
        if sig == Signal::INVALID {
            return;
        }
        let current = ProcessManager::current_pcb();
        let _ = sig.send_signal_info_to_pcb(None, current, PidType::PID);
    }

    /// 清除调试器为单步置位的 RFLAGS.TF
    #[cfg(target_arch = "x86_64")]
    fn disable_single_step(&self) {
        // 全程持锁：清标志、复验与写帧在同一临界区内完成
        let mut ps = self.ptrace_state.lock_irqsave();
        if !ps.forced_trap_flag {
            return;
        }
        ps.forced_trap_flag = false;
        // 仅在 tracee 仍处于 ptrace-stop（TrapFrame 稳定）时才写 frame.rflags；
        // 运行中 tracee 的 stop_frame_on_syscall_stack 是陈旧值，
        // 写它既无效又与该 CPU 的 entry 路径并发写 rflags 竞争。
        if Self::trap_frame_stable_locked(self, &ps) {
            // SAFETY: 复验通过，帧稳定。
            let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            frame.rflags &= !X86_EFLAGS_TF; // 清 X86_EFLAGS_TF
        }
    }

    /// 在 ptrace_state 锁内复验 tracee 仍处于 ptrace-stop 后置 RFLAGS.TF，
    /// 并记录 forced_trap_flag。致命信号可能在请求校验之后唤醒 tracee，
    /// 届时 stop 帧已失效，拒绝写入。
    #[cfg(target_arch = "x86_64")]
    fn arm_trap_flag_single_step(&self) -> Result<(), SystemError> {
        let mut ps = self.ptrace_state.lock_irqsave();
        if !Self::trap_frame_stable_locked(self, &ps) {
            return Err(SystemError::ESRCH);
        }
        // SAFETY: 复验通过，帧稳定。
        let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
        frame.rflags |= X86_EFLAGS_TF; // 置 X86_EFLAGS_TF
        ps.forced_trap_flag = true;
        Ok(())
    }

    /// 非x86_64架构无硬件单步机制，清除单步为空操作。
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    fn disable_single_step(&self) {}

    /// 恢复 tracee 执行（CONT/SYSCALL/SINGLESTEP/SYSEMU）。
    /// 设置/清除 TRACE_* 工作位，存 injected_signal，唤醒 tracee 脱离 ptrace_stop。
    pub fn ptrace_resume(
        &self,
        request: PtraceRequest,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        // 先校验注入信号
        let resume_signal = match signal {
            None => Signal::INVALID,
            Some(Signal::INVALID) => return Err(SystemError::EIO),
            Some(s) => s,
        };

        // 设置/清除 syscall-trace / 单步工作位。
        match request {
            PtraceRequest::Singlestep => {
                // 置 RFLAGS.TF 前在锁内复验 tracee 仍处于 ptrace-stop：
                // 致命信号可能已将其唤醒，stop 帧随之失效。
                #[cfg(target_arch = "x86_64")]
                self.arm_trap_flag_single_step()?;
                self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                self.flags()
                    .remove(ProcessFlags::TRACE_SYSCALL | ProcessFlags::TRACE_SYSEMU);
            }
            PtraceRequest::Syscall => {
                self.flags().insert(ProcessFlags::TRACE_SYSCALL);
                self.flags()
                    .remove(ProcessFlags::TRACE_SINGLESTEP | ProcessFlags::TRACE_SYSEMU);
                self.disable_single_step();
            }
            PtraceRequest::Sysemu | PtraceRequest::SysemuSinglestep => {
                self.flags().insert(ProcessFlags::TRACE_SYSEMU);
                if request == PtraceRequest::SysemuSinglestep {
                    self.flags().insert(ProcessFlags::TRACE_SINGLESTEP);
                    // SYSEMU_SINGLESTEP 也需置硬件单步（锁内复验后写入）
                    #[cfg(target_arch = "x86_64")]
                    self.arm_trap_flag_single_step()?;
                } else {
                    self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);
                    self.disable_single_step();
                }
                self.flags().remove(ProcessFlags::TRACE_SYSCALL);
            }
            PtraceRequest::Cont => {
                self.flags().remove(
                    ProcessFlags::TRACE_SYSCALL
                        | ProcessFlags::TRACE_SINGLESTEP
                        | ProcessFlags::TRACE_SYSEMU,
                );
                self.disable_single_step();
            }
            _ => return Err(SystemError::EINVAL),
        }

        // 存注入信号，清 stop 标志，唤醒 tracee。
        let was_in_stop = {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.injected_signal = resume_signal;
            ps.listening = false;
            ps.stop_report_pending = false;
            ps.in_ptrace_stop = false; // 退出 ptrace-stop（修复 audit P0）
            self.sched_info().state().is_stopped()
        };

        if was_in_stop {
            if let Some(strong) = self.self_ref.upgrade() {
                // 从 Stopped 唤醒回 Runnable
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
        Ok(0)
    }
    /// 系统调用入口/出口的 ptrace-stop（热路径）。
    /// 返回 true 表示应跳过 syscall 执行（仅 SYSEMU 入口停止意义）；其余情况 false。
    #[inline]
    pub fn ptrace_report_syscall(&self, is_entry: bool, nr: u64, args: &[usize; 6]) -> bool {
        let f = self.flags().load();
        let traced = if is_entry {
            f.contains(ProcessFlags::TRACE_SYSCALL) || f.contains(ProcessFlags::TRACE_SYSEMU)
        } else {
            f.contains(ProcessFlags::TRACE_SYSCALL) || f.contains(ProcessFlags::TRACE_SINGLESTEP)
        };
        if !traced {
            return false;
        }
        self.ptrace_report_syscall_slow(is_entry, nr, args, f)
    }

    /// ptrace_report_syscall 的慢路径：tracee 已启用 syscall-trace / 单步，
    /// 需要构造 ptrace-stop、与 tracer 同步。仅在热路径早退未命中时调用。
    #[cold]
    #[inline(never)]
    fn ptrace_report_syscall_slow(
        &self,
        is_entry: bool,
        _nr: u64,
        _args: &[usize; 6],
        flags: ProcessFlags,
    ) -> bool {
        let is_single_step = flags.contains(ProcessFlags::TRACE_SINGLESTEP);
        let msg = if is_entry {
            PTRACE_EVENTMSG_SYSCALL_ENTRY
        } else {
            PTRACE_EVENTMSG_SYSCALL_EXIT
        };
        let sysgood = self
            .ptrace_state
            .lock_irqsave()
            .options
            .contains(PtraceOptions::TRACESYSGOOD);
        let sysemu_skip = is_entry && flags.contains(ProcessFlags::TRACE_SYSEMU);
        // 单步跨 syscall 出口：报告单步 trap
        if !is_entry && is_single_step {
            if let Ok(signr) = Self::ptrace_notify(Signal::SIGTRAP as usize, TRAP_TRACE) {
                Self::reinject_ptrace_signal(signr);
            }
        } else {
            // 纯 syscall-stop（entry 或非单步的 exit）：sysgood 位仅加于纯 syscall-stop。
            self.ptrace_state.lock_irqsave().event_message = msg;
            let exit_code = if sysgood {
                Signal::SIGTRAP as usize | PTRACE_SYSGOOD_BIT
            } else {
                Signal::SIGTRAP as usize
            };
            if let Ok(signr) = Self::ptrace_notify(exit_code, exit_code as i32) {
                Self::reinject_ptrace_signal(signr);
            }
        }
        sysemu_skip
    }
}
