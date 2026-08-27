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
    ipc::{
        sighand::ReapTransition,
        signal_types::{SigCode, SigInfo, SigType, SignalFlags},
    },
    mm::{remote_access::RemoteAccess, MemoryManagementArch},
    process::{
        cred, namespace::user_namespace::map_id_up, pid::PidType, KernelStack, ProcessState,
    },
    sched::{schedule, SchedMode},
};
use alloc::{sync::Arc, vec::Vec};
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
    let signr = pcb.ptrace_stop(original as usize, SigChildCode::Trapped, 0, info.as_mut());

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
/// x86 EFLAGS Resume Flag（恢复标志）位。
pub const X86_EFLAGS_RF: u64 = 1 << 16;
/// x86 DR6 保留位。ptrace 对外暴露正极性 virtual_dr6，与硬件 DR6 互转时按此掩码翻转。
pub(crate) const DR6_RESERVED: u64 = 0xffff_0ff0;
/// x86_64 DR7 保留位掩码（含 GD 位）
/// 加载到硬件前必须清除：保留位置 1 行为未定义，GD 位置 1 会在内核访问调试寄存器时触发 #DB。
pub(crate) const DR_CONTROL_RESERVED: u64 = 0xffff_ffff_0000_fc00;

/// 校验一个硬件断点 slot 的配置与地址组合
#[cfg(target_arch = "x86_64")]
fn validate_dr_slot(nibble: u64, addr: u64) -> Result<(), SystemError> {
    let rw = nibble & 0b11;
    let len_bits = (nibble >> 2) & 0b11;
    if rw == 0b10 {
        return Err(SystemError::EINVAL);
    }
    // 执行断点只支持 1 字节长度。
    if rw == 0b00 && len_bits != 0 {
        return Err(SystemError::EINVAL);
    }
    let len = match len_bits {
        0b00 => 1u64,
        0b01 => 2,
        0b10 => 8,
        _ => 4,
    };
    let user_end = MMArch::USER_END_VADDR.data() as u64;
    if addr >= user_end {
        return Err(SystemError::EINVAL);
    }
    // 断点地址须按其长度对齐。
    if addr & (len - 1) != 0 {
        return Err(SystemError::EINVAL);
    }
    // 断点区间终点不得越过用户地址空间上界。
    let end = addr.checked_add(len - 1).ok_or(SystemError::EINVAL)?;
    if end >= user_end {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

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

/// 一次 ptrace-stop 的完整快照。事件消息和可变 siginfo 必须与
/// generation 共同发布，不得用独立字段拼凑不同 stop 的状态。
#[derive(Debug)]
struct PtraceStopRecord {
    generation: u64,
    exit_code: usize,
    mutable_siginfo: Option<SigInfo>,
    event_message: usize,
    report_pending: bool,
}

/// tracer 已消费某一代 stop，但 tracee 尚未从 schedule() 返回。
/// generation 使旧 waiter 只能取走自己的 resume 结果。
#[derive(Debug)]
struct PtraceResumeRecord {
    generation: u64,
    injected_signal: Signal,
    mutable_siginfo: Option<SigInfo>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct PendingDebugSignal {
    pub bits: u64,
    pub icebp: bool,
    pub addr: usize,
}

#[derive(Debug, Clone, Copy)]
struct PendingDebugRecord {
    owned_bits: u64,
    unowned_bits: u64,
    icebp: bool,
    addr: usize,
    owner_generation: u64,
}

// PtraceState —— 跟踪状态机（对应 Linux task_struct 的 ptrace/jobctl 相关字段）
/// 进程被 ptrace 跟踪时的状态信息。
#[derive(Debug)]
pub struct PtraceState {
    current_stop: Option<PtraceStopRecord>,
    completed_resume: Option<PtraceResumeRecord>,
    next_stop_generation: u64,
    /// ptrace 选项（PTRACE_O_*）。
    pub options: PtraceOptions,
    /// PTRACE_LISTEN：tracee 处于 STOP trap 但不被 wait 视为 TRACED。
    pub listening: bool,
    /// 持久标志：tracee 当前是否处于 ptrace-stop
    pub in_ptrace_stop: bool,
    /// 请求级冻结
    pub frozen: bool,
    /// 冻结期间曾有致命信号被门控延迟
    pub deferred_fatal_wake: bool,
    /// attach 到已 STOPPED 任务时挂起的 PTRACE_EVENT_STOP 信号。
    pub pending_event_stop: Option<Signal>,
    /// TIF_FORCED_TF：true 表示当前 TF 是调试器为 single-step 强制置位。
    pub forced_trap_flag: bool,
    /// 当前 ptrace-stop 的用户 TrapFrame 是否位于 syscall 栈。
    /// 调度上下文保存的 rsp 不能用来猜 TrapFrame 位置。
    pub stop_frame_on_syscall_stack: bool,
    /// EXITKILL 判定位（doom 位）：旧 tracer 退出且本会话设了
    /// PTRACE_O_EXITKILL 时，在关系清除的同一临界区内置位，
    /// 表示“该 tracee 已被旧会话判死，SIGKILL 待发”。
    pub exitkill_pending: bool,
    /// 调试寄存器（DR0-DR7）的 ptrace 侧存储。
    pub debug_regs: [u64; 8],
    /// Fixed-size #DB handoff from exception context to return-to-user.
    pending_debug: Option<PendingDebugRecord>,
}

impl Default for PtraceState {
    fn default() -> Self {
        Self {
            current_stop: None,
            completed_resume: None,
            next_stop_generation: 0,
            options: PtraceOptions::empty(),
            listening: false,
            in_ptrace_stop: false,
            frozen: false,
            deferred_fatal_wake: false,
            pending_event_stop: None,
            forced_trap_flag: false,
            stop_frame_on_syscall_stack: false,
            exitkill_pending: false,
            debug_regs: [0; 8],
            pending_debug: None,
        }
    }
}

impl PtraceState {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn consume_stop_report(&mut self, consume: bool) -> Option<i32> {
        if self.listening {
            return None;
        }
        let stop = self.current_stop.as_mut()?;
        if !stop.report_pending {
            return None;
        }
        let code = stop.exit_code as i32;
        if consume {
            stop.report_pending = false;
        }
        Some(code)
    }

    fn publish_stop(
        &mut self,
        exit_code: usize,
        mutable_siginfo: Option<SigInfo>,
        event_message: usize,
    ) -> u64 {
        self.next_stop_generation = self.next_stop_generation.wrapping_add(1);
        if self.next_stop_generation == 0 {
            self.next_stop_generation = 1;
        }
        let generation = self.next_stop_generation;
        self.current_stop = Some(PtraceStopRecord {
            generation,
            exit_code,
            mutable_siginfo,
            event_message,
            report_pending: true,
        });
        generation
    }

    fn prepare_resume(&mut self, injected_signal: Signal) -> Result<(), SystemError> {
        let stop = self.current_stop.take().ok_or(SystemError::ESRCH)?;
        // 同一 tracee 在从旧 schedule() 返回前不能再消费新 stop；
        // 拒绝覆盖可保证旧 waiter 永远不会误取新代结果。
        if self.completed_resume.is_some() {
            self.current_stop = Some(stop);
            return Err(SystemError::ESRCH);
        }
        self.completed_resume = Some(PtraceResumeRecord {
            generation: stop.generation,
            injected_signal,
            mutable_siginfo: stop.mutable_siginfo,
        });
        Ok(())
    }

    fn finish_waiter(&mut self, generation: u64) -> (Option<SigInfo>, Signal) {
        if self
            .completed_resume
            .as_ref()
            .map(|resume| resume.generation == generation)
            .unwrap_or(false)
        {
            let resume = self.completed_resume.take().unwrap();
            return (resume.mutable_siginfo, resume.injected_signal);
        }
        if self
            .current_stop
            .as_ref()
            .map(|stop| stop.generation == generation)
            .unwrap_or(false)
        {
            let stop = self.current_stop.take().unwrap();
            return (stop.mutable_siginfo, Signal::INVALID);
        }
        // 新代 stop 已发布时绝不清理它；旧 waiter 无注入信号返回。
        (None, Signal::INVALID)
    }

    fn stop_siginfo(&self) -> Option<SigInfo> {
        self.current_stop
            .as_ref()
            .and_then(|stop| stop.mutable_siginfo)
    }

    fn stop_siginfo_mut(&mut self) -> Option<&mut SigInfo> {
        self.current_stop
            .as_mut()
            .and_then(|stop| stop.mutable_siginfo.as_mut())
    }

    fn stop_event_message(&self) -> usize {
        self.current_stop
            .as_ref()
            .map(|stop| stop.event_message)
            .unwrap_or(0)
    }

    /// 解除 ptrace 会话时的唯一 stop/reset 入口。
    fn reset_session_stop(&mut self) {
        // 阻塞在 ptrace_stop() 的 waiter 需要一份 generation-bound
        // 结果才能安全返回。
        if let Some(stop) = self.current_stop.take() {
            if self.completed_resume.is_none() {
                self.completed_resume = Some(PtraceResumeRecord {
                    generation: stop.generation,
                    injected_signal: Signal::INVALID,
                    mutable_siginfo: stop.mutable_siginfo,
                });
            }
        }
        self.in_ptrace_stop = false;
        self.frozen = false;
        self.deferred_fatal_wake = false;
        self.listening = false;
        self.pending_event_stop = None;
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
    let parent_cred = parent.cred();
    let child_cred = child.cred();
    let allowed = parent_cred
        .has_capability_in_ns(&child_cred.user_ns, cred::CAPFlags::CAP_SYS_PTRACE)
        || (Arc::ptr_eq(&parent_cred.user_ns, &child_cred.user_ns)
            && (child_cred.cap_permitted.bits() & !parent_cred.cap_permitted.bits()) == 0);
    if !allowed {
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

const NO_PTRACE_SLOT: usize = usize::MAX;

/// Install both sides of a ptrace relation.  The caller must hold
/// `PTRACE_RELATION_LOCK`.
fn link_relation_locked(
    tracee: &Arc<ProcessControlBlock>,
    tracer: &Arc<ProcessControlBlock>,
) -> Result<(), SystemError> {
    if tracee.ptracer_pcb.read_irqsave().upgrade().is_some() {
        return Err(SystemError::EPERM);
    }

    let slot = {
        let mut tracees = tracer.ptraced.write_irqsave();
        assert!(
            tracees.len() < tracees.capacity(),
            "ptrace relation link entered irqsave lock without reserved capacity"
        );
        let slot = tracees.len();
        tracees.push(tracee.clone());
        slot
    };
    tracee.ptrace_slot.store(slot, Ordering::Relaxed);
    tracee.advance_ptrace_session_generation();
    *tracee.ptracer_pcb.write_irqsave() = Arc::downgrade(tracer);
    tracee.flags().insert(ProcessFlags::PTRACED);
    Ok(())
}

/// Reserve admission capacity without holding the global irqsave relation
/// lock. A concurrent linker may consume it, so every caller must recheck
/// `len < capacity` after reacquiring `PTRACE_RELATION_LOCK` and retry.
fn reserve_relation_slot(tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    tracer
        .ptraced
        .write()
        .try_reserve(1)
        .map_err(|_| SystemError::ENOMEM)
}

fn relation_slot_available_locked(tracer: &Arc<ProcessControlBlock>) -> bool {
    let tracees = tracer.ptraced.read_irqsave();
    tracees.len() < tracees.capacity()
}

/// Remove both sides of a ptrace relation in O(1).  The caller must hold
/// `PTRACE_RELATION_LOCK`.
fn unlink_relation_locked(tracee: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    let tracer = tracee.ptracer_pcb.read_irqsave().upgrade()?;
    let slot = tracee.ptrace_slot.load(Ordering::Relaxed);
    let moved = {
        let mut tracees = tracer.ptraced.write_irqsave();
        assert!(slot < tracees.len(), "ptrace relation slot out of bounds");
        assert!(
            Arc::ptr_eq(&tracees[slot], tracee),
            "ptrace relation slot points at another tracee"
        );
        tracees.swap_remove(slot);
        tracees.get(slot).cloned()
    };
    if let Some(moved) = moved {
        moved.ptrace_slot.store(slot, Ordering::Relaxed);
    }

    tracee.advance_ptrace_session_generation();
    tracee.ptrace_slot.store(NO_PTRACE_SLOT, Ordering::Relaxed);
    *tracee.ptracer_pcb.write_irqsave() = alloc::sync::Weak::new();
    tracee.flags().remove(ProcessFlags::PTRACED);
    Some(tracer)
}

/// Pop one relation owned by `tracer` without allocating.  The caller must
/// hold `PTRACE_RELATION_LOCK`.
fn pop_tracee_locked(tracer: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>> {
    let tracee = tracer.ptraced.write_irqsave().pop()?;
    let expected_slot = tracer.ptraced.read_irqsave().len();
    assert_eq!(
        tracee.ptrace_slot.load(Ordering::Relaxed),
        expected_slot,
        "popped ptrace relation has a stale slot"
    );
    assert!(
        tracee
            .ptracer_pcb
            .read_irqsave()
            .upgrade()
            .map(|owner| Arc::ptr_eq(&owner, tracer))
            .unwrap_or(false),
        "popped tracee belongs to another tracer"
    );
    tracee.advance_ptrace_session_generation();
    tracee.ptrace_slot.store(NO_PTRACE_SLOT, Ordering::Relaxed);
    *tracee.ptracer_pcb.write_irqsave() = alloc::sync::Weak::new();
    tracee.flags().remove(ProcessFlags::PTRACED);
    Some(tracee)
}

pub(crate) enum PtraceZombieClaim {
    Claimed { need_cascade: bool },
    Blocked,
    Lost,
}

/// Atomically validate wait ownership, claim the zombie, and unlink the
/// ptrace relation. This mirrors Linux's EXIT_ZOMBIE -> EXIT_TRACE transition
/// while tasklist_lock still protects the relationship.
pub(crate) fn claim_and_unlink_wait_zombie(
    tracee: &Arc<ProcessControlBlock>,
    waiter: &Arc<ProcessControlBlock>,
    options: WaitOption,
) -> PtraceZombieClaim {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let Some(tracer) = ptracer_of_locked(tracee) else {
        return PtraceZombieClaim::Lost;
    };
    let same_waiter = Arc::ptr_eq(&tracer, waiter);
    let same_thread_group = !options.contains(WaitOption::WNOTHREAD) && tracer.tgid == waiter.tgid;
    if !same_waiter && !same_thread_group {
        return PtraceZombieClaim::Lost;
    }

    match tracee.sighand().try_claim_ptraced_child(tracee) {
        ReapTransition::Blocked => return PtraceZombieClaim::Blocked,
        ReapTransition::TraceClaimed => {}
        _ => return PtraceZombieClaim::Lost,
    }

    let need_cascade = tracee
        .real_parent_pcb()
        .map(|real_parent| tracer.raw_tgid() != real_parent.raw_tgid())
        .unwrap_or(false);
    let owner = unlink_relation_locked(tracee);
    debug_assert!(
        owner
            .as_ref()
            .map(|owner| Arc::ptr_eq(owner, &tracer))
            .unwrap_or(false),
        "ptrace zombie owner changed while relation lock was held"
    );

    tracee.flags().remove(
        ProcessFlags::TRACE_SYSCALL
            | ProcessFlags::TRACE_SINGLESTEP
            | ProcessFlags::TRACE_SYSEMU
            | ProcessFlags::PT_SEIZED
            | ProcessFlags::PTRACE_EVENT_STOP
            | ProcessFlags::PENDING_PTRACE_STOP
            | ProcessFlags::TRAPPING,
    );
    let mut ps = tracee.ptrace_state.lock_irqsave();
    ps.reset_session_stop();
    ps.options = PtraceOptions::empty();

    PtraceZombieClaim::Claimed { need_cascade }
}

pub fn traceme_current() -> Result<(), SystemError> {
    let current = ProcessManager::current_pcb();
    loop {
        let reserve_for = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let tracer = traceme_parent_for(&current)?;
            traceme_allowed(&tracer, &current)?;
            if relation_slot_available_locked(&tracer) {
                link_relation_locked(&current, &tracer)?;
                break;
            }
            tracer
        };
        reserve_relation_slot(&reserve_for)?;
    }
    // 关系锁已随上块释放：若本进程携带旧 tracer 退出时遗留的 EXITKILL
    // 判定，在此接管执行（不可持锁发送，见 carry_out_pending_exitkill）。
    carry_out_pending_exitkill(&current);
    Ok(())
}

pub fn unlink_tracee(tracee: &Arc<ProcessControlBlock>) {
    let tracer = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        unlink_relation_locked(tracee)
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

/// 消费 tracee 的 EXITKILL doom 位（读并清）。
/// 调用方必须已持有 `PTRACE_RELATION_LOCK`：消费与关系状态的变更
/// 在同一临界区内互斥，保证 doom 位在全部消费方之间恰好被一方取得。
fn consume_exitkill_doom_locked(tracee: &ProcessControlBlock) -> bool {
    let mut ps = tracee.ptrace_state.lock_irqsave();
    core::mem::take(&mut ps.exitkill_pending)
}

/// 接管并执行旧会话遗留的 EXITKILL 判定。
/// 在新跟踪关系建立（attach/seize/traceme 成功）或 attach 失败回滚之后调用：
/// 若被跟踪者携带旧 tracer 退出时判定的 doom 位，在此消费并向其发送
/// SIGKILL——对应 Linux 中 attach 到已 SIGKILL-pending 的任务：attach 成功、
/// 任务随后死亡。必须在 `PTRACE_RELATION_LOCK` 之外调用（内部自行获取；
/// SIGKILL 发送链含内存分配与调度器锁，不能在关 IRQ 自旋临界区内执行）。
fn carry_out_pending_exitkill(tracee: &ProcessControlBlock) {
    let doomed = {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
        consume_exitkill_doom_locked(tracee)
    };
    if !doomed {
        return;
    }
    if let Some(strong) = tracee.self_ref.upgrade() {
        let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, strong, PidType::PID);
    }
}

/// 退出/销毁 tracer 时解除其所有 tracee 的跟踪关系。
pub fn exit_ptrace(tracer: &Arc<ProcessControlBlock>) {
    // Pop one relation per transaction.  Unlike the old `mem::take + Vec`
    // snapshot this is an allocation-free, O(1)-per-tracee exit path.
    loop {
        let pending = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let Some(tracee) = pop_tracee_locked(tracer) else {
                break;
            };

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
                ps.reset_session_stop();
                // 清空 ptrace 选项，与 ptrace_unlink 对称：避免 tracee 被新 tracer
                // attach 后继承本会话遗留选项（EXITKILL 等）。
                ps.options = PtraceOptions::empty();
                // EXITKILL 判定与关系清除在同一临界区发布：doom 位置位后，
                // SIGKILL 的发送权交给消费到该位的一方（阶段二，或随后建立
                // 新跟踪关系的 attach/seize/traceme）。
                if exitkill {
                    ps.exitkill_pending = true;
                }
            }
            tracee.flags().remove(
                ProcessFlags::PTRACE_EVENT_STOP
                    | ProcessFlags::PENDING_PTRACE_STOP
                    | ProcessFlags::TRAPPING,
            );

            // real_parent 锁内取一次 Arc clone，保活到锁外使用。
            let real_parent = tracee.real_parent_pcb();

            ExitPtracePending {
                tracee,
                exitkill,
                in_ptrace_stop,
                group_stop_active,
                real_parent,
            }
        };

        // 阶段二：脱离 PTRACE_RELATION_LOCK 后执行发 SIGKILL 与唤醒副作用。
        // 注意：phase1 清除关系后、phase2 执行前，并发 PTRACE_ATTACH 可能重新 attach 该 tracee。
        // PTRACE_RELATION_LOCK 是关 IRQ 自旋锁，不能跨 send_signal/wakeup 持有（会调度/取其它锁），
        // 故无法像 Linux tasklist_lock 那样跨整个 exit_ptrace 原子化。
        // EXITKILL 的判定与发送因此用 doom 位事务化：阶段一在清除关系的同一临界区
        // 置 exitkill_pending，发送权属于消费到该位的一方。此处消费时同时要求
        // 仍是 orphan（未被并发 attach 接管）——doom 消费与关系检查在同一
        // 临界区内原子完成，若已被 re-attach 则消费权留给 attach 侧
        // （carry_out_pending_exitkill），本会话不再发送，闭合误杀窗口。
        let ExitPtracePending {
            tracee,
            exitkill,
            in_ptrace_stop,
            group_stop_active,
            real_parent,
        } = pending;
        let (still_orphan, doomed) = {
            let _g = super::PTRACE_RELATION_LOCK.lock_irqsave();
            let orphan = !super::ptrace::is_ptraced_locked(&tracee);
            // 仅 orphan 时才消费 doom：被 re-attach 的 tracee 由 attach 侧接管。
            let doomed = orphan && exitkill && consume_exitkill_doom_locked(&tracee);
            (orphan, doomed)
        };
        if !still_orphan {
            // 已被并发 ATTACH 重新跟踪：新 tracer 拥有该 tracee，跳过本会话的副作用。
            continue;
        }
        if doomed {
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
    tracer
        .ptraced
        .read_irqsave()
        .iter()
        .map(|tracee| tracee.raw_pid())
        .collect()
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

/// Atomically validate that a deferred ptrace-owned event still belongs to
/// the currently installed tracing relation. A detach followed by reattach
/// must not hand an old event to the new tracer.
pub(crate) fn ptrace_session_matches(tracee: &ProcessControlBlock, generation: u64) -> bool {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    tracee.ptrace_session_generation() == generation && is_ptraced_locked(tracee)
}

/// Snapshot debug-event ownership under the relation lock.  The CPU debug
/// shadow may predate a running PTRACE_SEIZE, so an active relation always
/// owns a subsequent hardware event at the relation's current generation.
pub(crate) fn ptrace_debug_session_snapshot(tracee: &ProcessControlBlock) -> (Option<u64>, u64) {
    let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
    let generation = tracee.ptrace_session_generation();
    (is_ptraced_locked(tracee).then_some(generation), generation)
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

    // Both directions are committed under the relation lock; once the
    // ptracer matches, a second O(N) tracer-index scan is redundant.
    true
}

/// 访问检查所用的调用者凭据来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtraceAccessCreds {
    /// procfs 等文件系统路径：以 fsuid/fsgid 与 effective cap 集判定
    FsCreds,
    /// 显式系统调用：以 real uid/gid 与 permitted cap 集判定
    RealCreds,
}

// PCB ptrace 方法 —— 关系建立/解除、attach/seize/detach、TRAPPING 同步
impl ProcessControlBlock {
    pub fn has_permission_to_trace(&self, tracee: &Self, creds: PtraceAccessCreds) -> bool {
        // 1. 同一线程组允许访问（自省）
        if self.tgid == tracee.tgid {
            return true;
        }

        let caller_cred = self.cred();
        let tracee_cred = tracee.cred();
        let same_user_ns = Arc::ptr_eq(&caller_cred.user_ns, &tracee_cred.user_ns);
        let tracee_mm = tracee.basic().user_vm();
        // 调用者身份按凭据模式选取。
        let (caller_uid, caller_gid) = match creds {
            PtraceAccessCreds::FsCreds => (caller_cred.fsuid, caller_cred.fsgid),
            PtraceAccessCreds::RealCreds => (caller_cred.uid, caller_cred.gid),
        };
        // 2. 凭证匹配 + dumpable
        let uid_match = caller_uid == tracee_cred.euid
            && caller_uid == tracee_cred.suid
            && caller_uid == tracee_cred.uid;
        let gid_match = caller_gid == tracee_cred.egid
            && caller_gid == tracee_cred.sgid
            && caller_gid == tracee_cred.gid;
        // 3. CAP_SYS_PTRACE：在目标（tracee）的 user_ns
        // 判定 capability，而非调用者自身 ns，避免子 user namespace 越权跟踪父 ns 进程。
        let has_cap_in_task_ns = || {
            caller_cred.has_capability_in_ns(&tracee_cred.user_ns, cred::CAPFlags::CAP_SYS_PTRACE)
        };

        // 读侧屏障：与凭据提交路径的写侧屏障配对——写侧先发布 dumpability
        // 再发布新凭据，读侧读完 tracee 凭据后、读 dumpable 前插入屏障，
        // 保证不会观察到"新凭据 + 旧 dumpable"的乱序窗口（降权瞬间 attach）。
        fence(Ordering::SeqCst);

        if !(has_cap_in_task_ns() || same_user_ns && uid_match && gid_match) {
            return false;
        }

        let dumpable = tracee_mm
            .as_ref()
            .map(|mm| mm.dumpable())
            .unwrap_or(cred::SUID_DUMP_DISABLE as u8);
        if dumpable != cred::SUID_DUMP_USER as u8 {
            let mm_user_ns = tracee_mm
                .as_ref()
                .map(|mm| mm.user_ns())
                .unwrap_or_else(|| {
                    crate::process::namespace::user_namespace::INIT_USER_NAMESPACE.clone()
                });
            if !caller_cred.has_capability_in_ns(&mm_user_ns, cred::CAPFlags::CAP_SYS_PTRACE) {
                return false;
            }
        }

        // 4. capability 子集门：目标 permitted ⊆ 调用者 cap 集（同一 user_ns）。
        let caller_caps = match creds {
            PtraceAccessCreds::FsCreds => caller_cred.cap_effective,
            PtraceAccessCreds::RealCreds => caller_cred.cap_permitted,
        };
        (same_user_ns && (tracee_cred.cap_permitted.bits() & !caller_caps.bits()) == 0)
            || has_cap_in_task_ns()
    }

    /// 建立跟踪关系（tracee 侧调用）。
    /// 调用者不必持 `PTRACE_RELATION_LOCK`函数会自行获取。
    pub fn ptrace_link(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds) {
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
        loop {
            {
                let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();

                if tracer.exit_state() != ExitState::Running
                    || tracer.flags().contains(ProcessFlags::EXITING)
                {
                    // fork inheritance must not make fork fail solely because
                    // its tracer crossed the EXITING gate. Explicit link gets
                    // the normal permission failure.
                    return if check_tracer_liveness {
                        Ok(())
                    } else {
                        Err(SystemError::EPERM)
                    };
                }

                // 拒绝正在退出/已退出的目标。
                if self.exit_state() != ExitState::Running {
                    return Err(SystemError::EPERM);
                }
                if self.ptracer_pcb.read_irqsave().upgrade().is_some() {
                    return Err(SystemError::EPERM);
                }
                if relation_slot_available_locked(tracer) {
                    let tracee = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
                    return link_relation_locked(&tracee, tracer);
                }
            }

            reserve_relation_slot(tracer)?;
            // Capacity is only an admission reservation. Another concurrent
            // linker may consume it before this task reacquires relation lock.
        }
    }

    /// 解除跟踪关系，并按 group-stop 状态恢复 tracee 执行状态。
    /// 从 ptraced 列表移除、清 syscall-trace 工作、按 group-stop 决定 TracedStopped→Stopped 或唤醒。
    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();

        // 取出 tracer 并以 swap_remove O(1) 清除双向关系。
        let me = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        let _tracer = unlink_relation_locked(&me);

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
            ps.reset_session_stop();
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

    /// 若 tracee 当前处于 group-stop（Stopped），排队 attach trap 并唤醒它
    /// 自行完成 STOPPED→TRACED。对齐 Linux ptrace_attach() 的 JOBCTL_TRAP_STOP。
    fn ptrace_arm_attach_trap_if_stopped(&self) -> bool {
        let stop_sig = self.sighand().stop_signal();

        {
            let _pi = self.sched_info().pi_lock_irqsave();
            if !self.sched_info().state().is_stopped() {
                return false;
            }
            self.ptrace_set_trapping();
            self.ptrace_state.lock_irqsave().pending_event_stop = Some(stop_sig);
            self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        }
        if let Some(strong) = self.self_ref.upgrade() {
            let _ = ProcessManager::wakeup_stop(&strong);
        }
        true
    }

    pub fn ptrace_attach(&self, tracer: &Arc<ProcessControlBlock>) -> Result<isize, SystemError> {
        let _exec_guard = self.exec_update_read();
        let is_same_thread_group = tracer.tgid == self.tgid;

        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds)
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
            // 目标已 group-stop：等待 tracee 自身提交 ptrace-stop 并清 TRAPPING。
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
                // attach 失败回滚：目标可能携带旧会话遗留的 EXITKILL 判定
                // （exit_ptrace 阶段二已因本次 link 而跳过），在此接管执行，
                // 避免 doom 位搁浅无人消费。
                let _ = self.ptrace_unlink();
                carry_out_pending_exitkill(self);
                return Err(e);
            }
        }
        // 停止协议完成后接管旧会话遗留的 EXITKILL 判定（若存在）：
        // 对应 Linux 中 attach 到已 SIGKILL-pending 的任务——attach 成功、
        // 任务随后死亡。放在此处而非 link 内，避免 TRAPPING 等待与死亡交错。
        carry_out_pending_exitkill(self);

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
        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds)
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
        // 接管旧会话遗留的 EXITKILL 判定（若存在），语义同 attach 尾部。
        carry_out_pending_exitkill(self);
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
            ps.prepare_resume(data_signal)?;
            ps.pending_event_stop = None;
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
        self.ptrace_state.lock_irqsave().stop_event_message()
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

    /// 等待 tracee 真正调度切出
    fn wait_tracee_descheduled(&self) {
        while self.sched_info().is_running() && self.sched_info().state().is_stopped() {
            core::hint::spin_loop();
        }
    }

    /// 读 tracee 的用户寄存器（PTRACE_GETREGS）。
    #[cfg(target_arch = "x86_64")]
    pub fn tracee_user_regs(&self) -> Result<UserRegsStruct, SystemError> {
        loop {
            self.wait_tracee_descheduled();
            let ps = self.ptrace_state.lock_irqsave();
            if !Self::trap_frame_stable_locked(self, &ps) {
                return Err(SystemError::ESRCH);
            }
            if self.sched_info().is_running() {
                // 等待后 tracee 又被唤醒并再次停在调度前窗口，重试。
                continue;
            }
            // SAFETY: 复验通过，tracee 仍处于 ptrace-stop，TrapFrame 稳定。
            let frame = unsafe { &*self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            let mut regs = UserRegsStruct::from_trap_frame(frame);
            // fs/gs base 不在 TrapFrame 中：从 ArchPCBInfo 权威存储读取（切出时已回读最新值）。
            {
                let arch = self.arch_info_irqsave();
                regs.fs_base = arch.fsbase() as u64;
                regs.gs_base = arch.gsbase() as u64;
            }
            if ps.forced_trap_flag {
                regs.flags &= !X86_EFLAGS_TF;
            }
            return Ok(regs);
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn write_tracee_user_regs(&self, regs: &UserRegsStruct) -> Result<(), SystemError> {
        loop {
            self.wait_tracee_descheduled();
            let mut ps = self.ptrace_state.lock_irqsave();
            if !Self::trap_frame_stable_locked(self, &ps) {
                return Err(SystemError::ESRCH);
            }
            if self.sched_info().is_running() {
                continue;
            }
            // fs/gs base 校验
            let user_end = MMArch::USER_END_VADDR.data() as u64;
            if regs.fs_base >= user_end || regs.gs_base >= user_end {
                return Err(SystemError::EIO);
            }
            // SAFETY: tracee 仍处于 ptrace-stop，TrapFrame 稳定。
            let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            regs.write_to_trap_frame(frame)?;
            // fs/gs base 写入 ArchPCBInfo 权威存储；只写内存不碰硬件，
            {
                let mut arch = self.arch_info_irqsave();
                arch.set_fsbase(regs.fs_base as usize);
                arch.set_gsbase(regs.gs_base as usize);
            }
            if frame.rflags & X86_EFLAGS_TF != 0 {
                ps.forced_trap_flag = false;
            } else if ps.forced_trap_flag {
                frame.rflags |= X86_EFLAGS_TF;
            }
            return Ok(());
        }
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
            let regs = self.tracee_user_regs()?;
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
            let mut regs = self.tracee_user_regs()?;
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

            let has_dr = {
                let mut ps = self.ptrace_state.lock_irqsave();
                match idx {
                    // DR0-3：地址寄存器。禁止越界值
                    0..=3 => {
                        if val >= MMArch::USER_END_VADDR.data() as u64 {
                            return Err(SystemError::EINVAL);
                        }
                        let dr7 = ps.debug_regs[7] & !DR_CONTROL_RESERVED;
                        if ((dr7 >> (idx * 2)) & 3) != 0 {
                            validate_dr_slot((dr7 >> (16 + idx * 4)) & 0xf, val)?;
                        }
                        ps.debug_regs[idx] = val;
                    }
                    // DR6(slot6) 存储正极性 virtual_dr6，写入时翻转。
                    6 => {
                        ps.debug_regs[6] = val ^ DR6_RESERVED;
                    }
                    // DR7：控制寄存器。同一临界区内"先全量校验、后提交
                    _ => {
                        let v = val & !DR_CONTROL_RESERVED;
                        for i in 0..4usize {
                            // 未启用且从未写过地址的 slot 没有可校验的
                            // 组合，跳过；已写入地址的 slot（组合状态存在）
                            // 无论启用与否都校验编码，保证任意写入顺序下
                            // 组合状态始终有效。
                            if ((v >> (i * 2)) & 3) == 0 && ps.debug_regs[i] == 0 {
                                continue;
                            }
                            validate_dr_slot((v >> (16 + i * 4)) & 0xf, ps.debug_regs[i])?;
                        }
                        ps.debug_regs[7] = val;
                    }
                }
                // 维护硬件断点快路径标志：任一地址寄存器（DR0-3）或控制寄存器（DR7）非零即视为有配置，上下文切换据此加载/清除。
                ps.debug_regs[0..4].iter().any(|&v| v != 0) || ps.debug_regs[7] != 0
            };
            if has_dr {
                self.flags().insert(ProcessFlags::HW_DEBUG_REGS);
            } else {
                self.flags().remove(ProcessFlags::HW_DEBUG_REGS);
            }
            return Ok(());
        }
        // 间隙区拒绝写入。
        Err(SystemError::EIO)
    }

    /// exec 成功后清空硬件断点配置
    pub fn flush_ptrace_hw_debug_regs(&self) {
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.debug_regs = [0; 8];
            ps.pending_debug = None;
        }
        self.flags()
            .remove(ProcessFlags::HW_DEBUG_REGS | ProcessFlags::PENDING_DEBUG);
        #[cfg(target_arch = "x86_64")]
        if ProcessManager::current_pcb().raw_pid() == self.raw_pid() {
            crate::arch::x86_64::process::clear_current_debug_regs(self);
        }
    }

    /// Record #DB causes without allocating or delivering a signal from the
    /// exception entry. `owned_bits` belong to the ptrace session identified
    /// by `owner_generation`; `unowned_bits` are user-originated TF/ICEBP
    /// causes and remain deliverable without a tracer.
    pub(crate) fn record_pending_debug(
        &self,
        owned_bits: u64,
        unowned_bits: u64,
        icebp: bool,
        addr: usize,
        owner_generation: u64,
    ) {
        let mut ps = self.ptrace_state.lock_irqsave();
        match ps.pending_debug.as_mut() {
            Some(pending) if pending.owner_generation == owner_generation => {
                pending.owned_bits |= owned_bits;
                pending.unowned_bits |= unowned_bits;
                pending.icebp |= icebp;
                if pending.addr == 0 {
                    pending.addr = addr;
                }
            }
            _ => {
                ps.pending_debug = Some(PendingDebugRecord {
                    owned_bits,
                    unowned_bits,
                    icebp,
                    addr,
                    owner_generation,
                });
            }
        }
        drop(ps);
        self.flags().insert(ProcessFlags::PENDING_DEBUG);
    }

    /// Consume pending #DB causes at return-to-user. Causes owned by a stale
    /// ptrace session are discarded; user-originated causes remain signals.
    pub(crate) fn take_pending_debug_signal(&self) -> Option<PendingDebugSignal> {
        let pending = self.ptrace_state.lock_irqsave().pending_debug.take()?;
        let owner_is_current =
            pending.owned_bits != 0 && ptrace_session_matches(self, pending.owner_generation);
        let owned_bits = if owner_is_current {
            pending.owned_bits
        } else {
            0
        };
        let bits = owned_bits | pending.unowned_bits;
        (bits != 0 || pending.icebp).then_some(PendingDebugSignal {
            bits,
            icebp: pending.icebp,
            addr: pending.addr,
        })
    }

    /// PTRACE_GETSIGINFO：读 last_siginfo。
    pub fn ptrace_get_siginfo(&self) -> Result<SigInfo, SystemError> {
        let ps = self.ptrace_state.lock_irqsave();
        ps.stop_siginfo().ok_or(SystemError::EINVAL)
    }

    /// PTRACE_SETSIGINFO：写 last_siginfo。
    pub fn ptrace_set_siginfo(&self, info: SigInfo) -> Result<(), SystemError> {
        let mut ps = self.ptrace_state.lock_irqsave();
        let slot = ps.stop_siginfo_mut().ok_or(SystemError::EINVAL)?;
        *slot = info;
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

    // PEEKDATA / POKEDATA —— 经 MM 层统一远程访问 API 读写 tracee 用户内存

    /// PTRACE_PEEKDATA/PEEKTEXT：读 tracee 用户空间一个 word。
    /// 正确处理跨页 word（addr 位于页尾时 8 字节跨两页）。
    pub fn ptrace_peek_data(&self, addr: usize) -> Result<usize, SystemError> {
        // 防回绕校验
        let last = addr
            .checked_add(size_of::<usize>() - 1)
            .ok_or(SystemError::EIO)?;
        if last >= MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EIO);
        }
        let _mm_guard = self.active_vm().ok_or(SystemError::ESRCH)?;
        let target_vm = _mm_guard.vm().clone();
        let mut bytes = [0u8; size_of::<usize>()];
        // 整 word 在单次 AddressSpace 读锁内拷（force=true：ptrace 越权语义）
        let n = target_vm.access_remote_vm(addr, RemoteAccess::Read(&mut bytes), true)?;
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
        let _mm_guard = self.active_vm().ok_or(SystemError::ESRCH)?;
        let target_vm = _mm_guard.vm().clone();
        let bytes = value.to_ne_bytes();
        let n = target_vm.access_remote_vm(addr, RemoteAccess::Write(&bytes), true)?;
        if n != size_of::<usize>() {
            return Err(SystemError::EIO);
        }
        Ok(())
    }

    /// PTRACE_GET_SYSCALL_INFO。
    /// 根据与 stop generation 同时发布的可变 siginfo 和 event message
    /// 决定 op，对齐 Linux 6.6 直接读 last_siginfo/ptrace_message 的语义。
    /// op 判定与帧读取在同一 ptrace_state 临界区内完成，且先复验 tracee
    /// 仍处于 ptrace-stop（复验失败返回 ESRCH），避免两者来自不同时刻的
    /// 拼凑快照；用户态拷贝由调用方在锁外进行。
    #[cfg(target_arch = "x86_64")]
    pub fn ptrace_get_syscall_info(&self) -> Result<PtraceSyscallInfo, SystemError> {
        let ps = self.ptrace_state.lock_irqsave();
        if !Self::trap_frame_stable_locked(self, &ps) {
            return Err(SystemError::ESRCH);
        }
        let code = ps
            .stop_siginfo()
            .map(|info| info.sig_code().as_i32())
            .unwrap_or(0);
        let msg = ps.stop_event_message();
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
        // 读 trap frame 填 ip/sp/nr/args。
        // SAFETY: 复验通过，tracee 仍处于 ptrace-stop，TrapFrame 稳定。
        let frame = unsafe { &*self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
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

    /// 请求级冻结 tracee 的 ptrace-stop
    pub fn ptrace_freeze(&self) -> Result<(), SystemError> {
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
            // SIGKILL 已挂起：拒绝冻结，tracee 将走死亡路径，
            // tracer 侧表现为常见的 ESRCH。
            return Err(SystemError::ESRCH);
        }
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            // 复验 tracee 仍在 ptrace-stop：fatal 唤醒可能已抢先清位。
            if !(ps.in_ptrace_stop && self.sched_info().state().is_stopped()) {
                return Err(SystemError::ESRCH);
            }
            ps.frozen = true;
        }
        Ok(())
        // 各守卫按声明逆序释放：ptrace_state → sig_info → sighand inner。
    }

    /// 解除请求级冻结并补发被延迟的致命唤醒。
    pub fn ptrace_unfreeze(&self) {
        let wake = {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.frozen = false;
            let wake = ps.deferred_fatal_wake;
            ps.deferred_fatal_wake = false;
            if wake && ps.in_ptrace_stop {
                ps.in_ptrace_stop = false;
            }
            wake
        };
        if wake {
            if let Some(strong) = self.self_ref.upgrade() {
                // 补发被门控延迟的死亡唤醒。wakeup_stop 对已 Runnable 目标
                // （如 CONT 已将其唤醒）早退，不会造成双重唤醒。
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
    }

    /// 进入 ptrace-stop
    fn ptrace_stop(
        &self,
        exit_code: usize,
        why: SigChildCode,
        event_message: usize,
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
        let generation = {
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
                None
            } else {
                let mut ps = self.ptrace_state.lock_irqsave();
                let mutable_siginfo = info.as_ref().map(|i| **i);
                let generation = ps.publish_stop(exit_code, mutable_siginfo, event_message);
                ps.listening = false;
                ps.in_ptrace_stop = true;
                #[cfg(target_arch = "x86_64")]
                {
                    ps.stop_frame_on_syscall_stack = self.current_stop_frame_on_syscall_stack();
                }
                drop(ps);
                self.sched_info().set_state(ProcessState::Stopped);
                Some(generation)
            }
            // siginfo_g 与 sighand_g 在 set_state 之后才 drop
        };

        drop(relation_guard);

        // 5. fence(Release)：保证 Stopped + exit_code 在 TRAPPING 清除前对 tracer 可见。
        fence(Ordering::Release);

        let Some(generation) = generation else {
            self.ptrace_clear_trapping();
            return 0;
        };

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
        let (saved_siginfo, injected) = ps.finish_waiter(generation);
        if let Some(i) = info {
            if let Some(saved) = saved_siginfo {
                // 回填：PTRACE_SETSIGINFO 的修改参与后续信号递送。
                *i = saved;
            }
        }
        // 只有本代仍是活动 stop 时才清理控制位；若其他 CPU
        // 已发布新代 stop，旧 waiter 不得破坏新 stop 的门控。
        let newer_stop = ps.current_stop.is_some();
        if !newer_stop {
            ps.listening = false;
            ps.in_ptrace_stop = false;
            ps.frozen = false;
        }
        let result = if injected != Signal::INVALID {
            injected as usize
        } else {
            0
        };
        drop(ps);

        // 唤醒后重算信号 pending（可能被 tracer 注入了信号）。
        if let Some(strong) = self.self_ref.upgrade() {
            strong.recalc_sigpending();
        }

        result
    }

    /// group-stop 路径的 typed 入口，避免调用方拼装内部 stop 原因。
    pub(crate) fn ptrace_group_stop(&self, signal: Signal) -> usize {
        self.ptrace_stop(signal as usize, SigChildCode::Stopped, 0, None)
    }

    /// 在 tracee 上下文消费一个 pending ptrace trap。
    /// 返回 true 表示已经处理，调用方应继续复验粘性 pending 位。
    pub fn ptrace_handle_pending_stop(&self) -> bool {
        if !self.flags().contains(ProcessFlags::PTRACED)
            || !self
                .flags()
                .test_and_clear(ProcessFlags::PENDING_PTRACE_STOP)
        {
            return false;
        }
        let pending_sig = self
            .ptrace_state
            .lock_irqsave()
            .pending_event_stop
            .take()
            .unwrap_or(Signal::SIGTRAP);
        if self.flags().contains(ProcessFlags::PT_SEIZED) {
            let _ = self.ptrace_event_stop(pending_sig);
        } else {
            // Linux do_jobctl_trap() 的普通 ATTACH group-stop 无 siginfo，
            // 且忽略 resume data。
            let _ = self.ptrace_group_stop(pending_sig);
        }
        true
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
            let exit_code = (event as usize) << EXITCODE_EVENT_SHIFT | Signal::SIGTRAP as usize;
            // 仅 signal-delivery-stop 与 syscall-stop 消费 ptrace_notify 返回的注入信号；
            // FORK/CLONE/VFORK/EXEC/EXIT/SECCOMP 事件不应向 tracee 重注入 tracer 经 CONT 传入的信号。
            let _ = Self::ptrace_notify_with_message(exit_code, exit_code as i32, message);
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
        self.ptrace_stop(exit_code, SigChildCode::Stopped, 0, Some(&mut info))
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
            .stop_siginfo()
            .map(|info| (info.sig_code().as_i32() >> 8) == PtraceEvent::Stop as i32)
            .unwrap_or(false);
        if !ps.in_ptrace_stop || !is_event_stop {
            return Err(SystemError::EIO);
        }
        // 取出触发本次 event-stop 的信号（低字节）。
        let stop_signal = Signal::from(
            ps.current_stop
                .as_ref()
                .map(|stop| stop.exit_code)
                .unwrap_or(0) as i32
                & EXITCODE_SIG_MASK as i32,
        );
        let retrap = ps.pending_event_stop.is_some();
        ps.listening = true;
        // 清 in_ptrace_stop：LISTEN 状态下 tracee 不在 ptrace-stop，
        // ptrace_check_attach 应返回 ESRCH
        ps.in_ptrace_stop = false;
        // 同临界区清请求级冻结
        ps.frozen = false;
        // 关键：清 stop_report_pending，使 wait 不再返回此 stop
        if let Some(stop) = ps.current_stop.as_mut() {
            stop.report_pending = false;
        }
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
    /// 无 event message 的通用 trap 入口。
    pub fn ptrace_notify(exit_code: usize, si_code: i32) -> Result<usize, SystemError> {
        Self::ptrace_notify_with_message(exit_code, si_code, 0)
    }

    fn ptrace_notify_with_message(
        exit_code: usize,
        si_code: i32,
        event_message: usize,
    ) -> Result<usize, SystemError> {
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
        let signr = current.ptrace_stop(
            exit_code,
            SigChildCode::Trapped,
            event_message,
            Some(&mut info),
        );
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
        let user_tf = frame.rflags & X86_EFLAGS_TF != 0 && !ps.forced_trap_flag;
        frame.rflags |= X86_EFLAGS_TF;
        ps.forced_trap_flag = !user_tf;
        Ok(())
    }

    /// Linux x86 get_signal() handoff: stop hardware single-step before
    /// constructing a signal frame. The caller reports an immediate ptrace
    /// SIGTRAP only after frame construction succeeds.
    #[cfg(target_arch = "x86_64")]
    pub(crate) fn prepare_single_step_signal_delivery(&self, frame: &mut TrapFrame) -> bool {
        if !self.flags().contains(ProcessFlags::TRACE_SINGLESTEP) {
            return false;
        }
        self.flags().remove(ProcessFlags::TRACE_SINGLESTEP);
        let mut ps = self.ptrace_state.lock_irqsave();
        if ps.forced_trap_flag {
            frame.rflags &= !X86_EFLAGS_TF;
            ps.forced_trap_flag = false;
        }
        true
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
            ps.prepare_resume(resume_signal)?;
            ps.listening = false;
            ps.in_ptrace_stop = false;
            ps.frozen = false;
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
            let exit_code = if sysgood {
                Signal::SIGTRAP as usize | PTRACE_SYSGOOD_BIT
            } else {
                Signal::SIGTRAP as usize
            };
            if let Ok(signr) = Self::ptrace_notify_with_message(exit_code, exit_code as i32, msg) {
                Self::reinject_ptrace_signal(signr);
            }
        }
        sysemu_skip
    }
}
