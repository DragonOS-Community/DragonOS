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

/// ptrace hook on the signal-delivery path: called after `do_signal` dequeues a signal, before action lookup.
pub fn ptrace_signal(
    pcb: &Arc<ProcessControlBlock>,
    original: Signal,
    info: &mut Option<SigInfo>,
) -> Option<Signal> {
    // SIGKILL never passes through ptrace_signal (defensive: do_signal already handles it on the kernel_only path).
    if original == Signal::SIGKILL {
        return Some(Signal::SIGKILL);
    }

    // Enter signal-delivery-stop. ptrace_stop internally bails out early on fatal signals, blocks, then cleans up after wakeup.
    let signr = pcb.ptrace_stop(original as usize, SigChildCode::Trapped, 0, info.as_mut());

    if signr == 0 {
        // The tracer discarded the signal.
        return None;
    }

    let injected = Signal::from(signr as i32);
    if injected == Signal::INVALID {
        return None;
    }

    // If the tracer changed the signal number, rebuild siginfo (source SI_USER).
    if let Some(i) = info {
        if injected as i32 != i.signo_i32() {
            let sender = crate::process::ptrace::ptracer_of(pcb).or_else(|| pcb.real_parent_pcb());
            let sender_vpid = sender
                .as_ref()
                .and_then(|parent| parent.task_pid_nr_ns(PidType::PID, Some(pcb.active_pid_ns())))
                .map(|p| p.data())
                .unwrap_or(0);
            // Fall back to overflowuid (default 65534) when the cross-user-namespace mapping fails
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

    // If the injected signal is blocked by the current mask, or a fatal signal is pending, requeue it and return None so do_signal continues dequeuing the next one
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

/// Request types for the ptrace system call.
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

/// ptrace event types (PTRACE_EVENT_*).
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
    /// PTRACE_EVENT_STOP (128): produced by seize/INTERRUPT/group-stop in seized mode.
    Stop = 128,
}

/// syscall message identifiers for `PTRACE_GET_SYSCALL_INFO`.
pub const PTRACE_EVENTMSG_SYSCALL_ENTRY: usize = 1;
pub const PTRACE_EVENTMSG_SYSCALL_EXIT: usize = 2;

// si_code values for SIGTRAP
// The ptrace tracer (gdb) distinguishes single-step / hardware breakpoint / software breakpoint via si_code.
pub const TRAP_BRKPT: i32 = 1; // software breakpoint (int3)
pub const TRAP_TRACE: i32 = 2; // single-step (RFLAGS.TF)
pub const SI_KERNEL: i32 = 0x80;
/// DragonOS-v does not implement BTS (branch tracing) yet; kept for future use.
#[allow(dead_code)]
pub const TRAP_BRANCH: i32 = 3; // branch trap (BTS)
pub const TRAP_HWBKPT: i32 = 4; // hardware breakpoint (DR0-3 hit)
/// Single-step bit (BS) in x86 DR6. When do_debug's error_code (=DR6) has this bit set, it indicates a single-step trap.
pub const X86_DR_BS: u64 = 1 << 14;
/// Hardware breakpoint hit bits (B0-B3) in x86 DR6.
pub const X86_DR_B_MASK: u64 = 0x0f;
/// x86 EFLAGS Trap Flag (single-step) bit.
pub const X86_EFLAGS_TF: u64 = 0x100;
/// x86 EFLAGS Resume Flag (resume flag) bit.
pub const X86_EFLAGS_RF: u64 = 1 << 16;
/// x86 DR6 reserved bits. ptrace exposes the positive-polarity virtual_dr6; this mask is applied when converting to/from the hardware DR6.
pub(crate) const DR6_RESERVED: u64 = 0xffff_0ff0;
/// x86_64 DR7 reserved-bit mask (including the GD bit)
/// Must be cleared before loading into hardware: setting a reserved bit is undefined behavior, and setting GD triggers #DB when the kernel accesses the debug registers.
pub(crate) const DR_CONTROL_RESERVED: u64 = 0xffff_ffff_0000_fc00;

/// Validate the configuration/address combination of a hardware breakpoint slot
#[cfg(target_arch = "x86_64")]
fn validate_dr_slot(nibble: u64, addr: u64) -> Result<(), SystemError> {
    let rw = nibble & 0b11;
    let len_bits = (nibble >> 2) & 0b11;
    if rw == 0b10 {
        return Err(SystemError::EINVAL);
    }
    // Execution breakpoints only support a 1-byte length.
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
    // The breakpoint address must be aligned to its length.
    if addr & (len - 1) != 0 {
        return Err(SystemError::EINVAL);
    }
    // The end of the breakpoint range must not cross the top of the user address space.
    let end = addr.checked_add(len - 1).ok_or(SystemError::EINVAL)?;
    if end >= user_end {
        return Err(SystemError::EINVAL);
    }
    Ok(())
}

// ptrace exit_code / si_code encoding
const EXITCODE_SIG_MASK: usize = 0x7f;
/// Shift of the event encoding within exit_code.
const EXITCODE_EVENT_SHIFT: u32 = 8;
/// sysgood flag bit for syscall-stop (requires PTRACE_O_TRACESYSGOOD).
const PTRACE_SYSGOOD_BIT: usize = 0x80;

/// Value of the ptrace_syscall_info.arch field on x86_64 (AUDIT_ARCH_X86_64 = EM_X86_64|__AUDIT_ARCH_64BIT|__AUDIT_ARCH_LE).
pub const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;
/// Linux MAX_ERRNO: a return value in [-MAX_ERRNO, -1] is treated as an error.
pub const MAX_ERRNO: i64 = 4095;

bitflags::bitflags! {
    /// ptrace options (PTRACE_O_*).
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

// PTRACE_GET_SYSCALL_INFO structures
/// Values of `ptrace_syscall_info.op`.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, Default)]
pub enum PtraceSyscallInfoOp {
    #[default]
    None = 0,
    Entry = 1,
    Exit = 2,
    Seccomp = 3,
}

/// `ptrace_syscall_info.entry`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoEntry {
    pub nr: u64,
    pub args: [u64; 6],
}

/// `ptrace_syscall_info.exit`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoExit {
    pub rval: i64,
    pub is_error: u8,
}

/// `ptrace_syscall_info.seccomp`.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct PtraceSyscallInfoSeccomp {
    pub nr: u64,
    pub args: [u64; 6],
    pub ret_data: u32,
}

/// The data union of `ptrace_syscall_info`.
#[repr(C)]
#[derive(Clone, Copy)]
pub union PtraceSyscallInfoData {
    pub entry: PtraceSyscallInfoEntry,
    pub exit: PtraceSyscallInfoExit,
    pub seccomp: PtraceSyscallInfoSeccomp,
}

impl Default for PtraceSyscallInfoData {
    fn default() -> Self {
        // SAFETY: All union fields are POD integer types, so zero-initialization is valid.
        unsafe { core::mem::zeroed() }
    }
}

/// `struct ptrace_syscall_info`.
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
        // SAFETY: Same as PtraceSyscallInfoData.
        unsafe { core::mem::zeroed() }
    }
}

impl PtraceSyscallInfo {
    /// Lowest-level constructor: fills in the architecture-independent op/arch/ip/sp, leaving data empty.
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

    /// Mark as Entry and fill in the syscall number and arguments.
    pub fn with_entry(mut self, nr: u64, args: [u64; 6]) -> Self {
        self.op = PtraceSyscallInfoOp::Entry;
        self.data.entry = PtraceSyscallInfoEntry { nr, args };
        self
    }

    /// Mark as Exit and fill in the return value and whether it is an error.
    pub fn with_exit(mut self, rval: i64, is_error: bool) -> Self {
        self.op = PtraceSyscallInfoOp::Exit;
        self.data.exit = PtraceSyscallInfoExit {
            rval,
            is_error: is_error as u8,
        };
        self
    }

    /// Mark as Seccomp.
    pub fn with_seccomp(mut self, nr: u64, args: [u64; 6], ret_data: u32) -> Self {
        self.op = PtraceSyscallInfoOp::Seccomp;
        self.data.seccomp = PtraceSyscallInfoSeccomp { nr, args, ret_data };
        self
    }
}

/// Determine whether a syscall return value is an error
#[inline(always)]
pub fn syscall_retval_is_error(retval: i64) -> bool {
    (-MAX_ERRNO..=-1).contains(&retval)
}

// x86_64 user registers
/// x86_64 `struct user_regs_struct`, used by PTRACE_GETREGS/SETREGS/PEEKUSER/POKEUSER.
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
    /// Syscall number (DragonOS TrapFrame's errcode field holds nr during a syscall).
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
    /// Construct from a TrapFrame (GETREGS path).
    /// Note that orig_ax takes TrapFrame.errcode (the syscall number during a syscall).
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

    /// Write back to a TrapFrame (SETREGS path).
    /// Safety checks:
    /// - cs/ss must be RPL=3 and non-zero (prevents ring-0 injection)
    /// - rflags only allows the FLAG_MASK bits through, preserving the frame's non-masked bits (prevents clearing IF and hanging user mode)
    pub fn write_to_trap_frame(&self, frame: &mut TrapFrame) -> Result<(), SystemError> {
        // Segment selector validation
        // cs/ss: RPL=3, non-zero (SEGMENT_RPL_MASK=0x3, USER_RPL=3).
        if (self.cs & 0x3) != 3 || self.cs == 0 {
            return Err(SystemError::EIO);
        }
        if (self.ss & 0x3) != 3 || self.ss == 0 {
            return Err(SystemError::EIO);
        }
        // rflags: preserve the frame's non-masked bits, only allow the FLAG_MASK bits through.
        // FLAG_MASK = FLAG_MASK_32 | NT = CF|PF|AF|ZF|SF|TF|DF|RF|AC|NT = 0x00054DD5.
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

/// ELF NT_PRSTATUS note type (used by PTRACE_GETREGSET/SETREGSET).
pub const NT_PRSTATUS: u32 = 1;

/// A complete snapshot of one ptrace-stop. The event message and mutable siginfo must be
/// published together with the generation; do not assemble state from different stops using independent fields.
#[derive(Debug)]
struct PtraceStopRecord {
    generation: u64,
    exit_code: usize,
    mutable_siginfo: Option<SigInfo>,
    event_message: usize,
    report_pending: bool,
}

/// The tracer has consumed one generation of stop, but the tracee has not yet returned from schedule().
/// The generation ensures an old waiter can only take away its own resume result.
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

// PtraceState -- tracking state machine (corresponding to the ptrace/jobctl-related fields of Linux task_struct)
/// State information for a process being traced by ptrace.
#[derive(Debug)]
pub struct PtraceState {
    current_stop: Option<PtraceStopRecord>,
    completed_resume: Option<PtraceResumeRecord>,
    next_stop_generation: u64,
    /// ptrace options (PTRACE_O_*).
    pub options: PtraceOptions,
    /// PTRACE_LISTEN: the tracee is in a STOP trap but is not considered TRACED by wait.
    pub listening: bool,
    /// Persistent flag: whether the tracee is currently in a ptrace-stop
    pub in_ptrace_stop: bool,
    /// Request-level freeze
    pub frozen: bool,
    /// A fatal signal was gated and deferred during the freeze
    pub deferred_fatal_wake: bool,
    /// Pending PTRACE_EVENT_STOP signal when attaching to an already-STOPPED task.
    pub pending_event_stop: Option<Signal>,
    /// TIF_FORCED_TF: true indicates the current TF was forcibly set by the debugger for single-step.
    pub forced_trap_flag: bool,
    /// Whether the user TrapFrame of the current ptrace-stop is on the syscall stack.
    /// The rsp saved by the scheduler context cannot be used to guess the TrapFrame location.
    pub stop_frame_on_syscall_stack: bool,
    /// EXITKILL verdict bit (doom bit): set within the same critical section that clears the
    /// relation when the old tracer exits and this session had PTRACE_O_EXITKILL set,
    /// meaning "this tracee has been sentenced to death by the old session; SIGKILL is pending".
    pub exitkill_pending: bool,
    /// ptrace-side storage for the debug registers (DR0-DR7).
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
        // The same tracee must not consume a new stop before returning from the old schedule();
        // refusing to overwrite guarantees an old waiter can never mistake the new-generation result.
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
        // Never clean up a new-generation stop once it has been published; an old waiter returns with no injected signal.
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

    /// The only stop/reset entry point when tearing down a ptrace session.
    fn reset_session_stop(&mut self) {
        // A waiter blocked in ptrace_stop() needs a generation-bound
        // result to return safely.
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
    // The relation lock has been released with the block above: if this process carries an EXITKILL
    // verdict left by its old tracer's exit, take it over and execute it here (must not send while holding the lock, see carry_out_pending_exitkill).
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

/// Per-tracee side-effect snapshot: phase one collects it while holding the lock, phase two executes it outside the lock when a tracer exits/is destroyed.
struct ExitPtracePending {
    tracee: Arc<ProcessControlBlock>,
    /// Whether SIGKILL must be sent to the tracee (the old session set PTRACE_O_EXITKILL).
    exitkill: bool,
    /// Whether the tracee is in a ptrace-stop.
    in_ptrace_stop: bool,
    /// Whether the tracee still has an active group-stop.
    group_stop_active: bool,
    /// real_parent kept alive past the lock via an Arc clone taken while holding the lock.
    real_parent: Option<Arc<ProcessControlBlock>>,
}

/// Consume the tracee's EXITKILL doom bit (read and clear).
/// The caller must already hold `PTRACE_RELATION_LOCK`: consuming the bit and mutating the relation state are
/// mutually exclusive within the same critical section, guaranteeing exactly one consumer obtains the doom bit.
fn consume_exitkill_doom_locked(tracee: &ProcessControlBlock) -> bool {
    let mut ps = tracee.ptrace_state.lock_irqsave();
    core::mem::take(&mut ps.exitkill_pending)
}

/// Take over and execute an EXITKILL verdict left by the old session.
/// Called after a new tracing relation is established (attach/seize/traceme succeeds) or after an attach
/// failure rolls back: if the tracee carries a doom bit verdict from its old tracer's exit, consume it here and
/// send SIGKILL -- corresponding to Linux attaching to an already SIGKILL-pending task: attach succeeds, and the
/// task then dies. Must be called outside `PTRACE_RELATION_LOCK` (it acquires the lock itself; the SIGKILL send path
/// involves memory allocation and scheduler locks, so it cannot run inside an IRQ-disabled spinlock critical section).
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

/// Tear down the tracing relation for all tracees when a tracer exits/is destroyed.
pub fn exit_ptrace(tracer: &Arc<ProcessControlBlock>) {
    // Pop one relation per transaction.  Unlike the old `mem::take + Vec`
    // snapshot this is an allocation-free, O(1)-per-tracee exit path.
    loop {
        let pending = {
            let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();
            let Some(tracee) = pop_tracee_locked(tracer) else {
                break;
            };

            // If the tracee has PTRACE_O_EXITKILL set, send it SIGKILL when the tracer exits
            // (capture this flag before clearing options).
            let exitkill = tracee
                .ptrace_state
                .lock_irqsave()
                .options
                .contains(PtraceOptions::EXITKILL);
            // Clear the syscall-trace/single-step working bits to avoid leftovers.
            tracee.flags().remove(
                ProcessFlags::TRACE_SYSCALL
                    | ProcessFlags::TRACE_SINGLESTEP
                    | ProcessFlags::TRACE_SYSEMU
                    | ProcessFlags::PT_SEIZED,
            );

            // Decision: whether the tracee is really in a ptrace-stop.
            let in_ptrace_stop = {
                let ps = tracee.ptrace_state.lock_irqsave();
                (ps.in_ptrace_stop || ps.listening) && tracee.sched_info().state().is_stopped()
            };
            // Unconditionally clear the single-step TF
            // A running tracee may have TF set via PTRACE_SINGLESTEP/SYSEMU_SINGLESTEP;
            // if the tracer exits without clearing it, the tracee will hit #DB after resuming and force_sig(SIGTRAP) kills it.
            tracee.disable_single_step();
            let group_stop_active = tracee.sighand().flags_contains(SignalFlags::STOP_STOPPED);

            // Clear the ptrace-side state of this stop.
            {
                let mut ps = tracee.ptrace_state.lock_irqsave();
                ps.reset_session_stop();
                // Clear the ptrace options, symmetric to ptrace_unlink: prevents the tracee from
                // inheriting this session's leftover options (EXITKILL, etc.) after being re-attached by a new tracer.
                ps.options = PtraceOptions::empty();
                // The EXITKILL verdict and the relation teardown are published in the same critical
                // section: once the doom bit is set, the right to send SIGKILL goes to whoever consumes
                // that bit (phase two, or a subsequent attach/seize/traceme that establishes a new relation).
                if exitkill {
                    ps.exitkill_pending = true;
                }
            }
            tracee.flags().remove(
                ProcessFlags::PTRACE_EVENT_STOP
                    | ProcessFlags::PENDING_PTRACE_STOP
                    | ProcessFlags::TRAPPING,
            );

            // Take one Arc clone of real_parent while holding the lock, keeping it alive for use outside the lock.
            let real_parent = tracee.real_parent_pcb();

            ExitPtracePending {
                tracee,
                exitkill,
                in_ptrace_stop,
                group_stop_active,
                real_parent,
            }
        };

        // Phase two: execute the SIGKILL send and wakeup side effects after leaving PTRACE_RELATION_LOCK.
        // Note: between phase1 clearing the relation and phase2 executing, a concurrent PTRACE_ATTACH may re-attach the tracee.
        // PTRACE_RELATION_LOCK is an IRQ-disabling spinlock and cannot be held across send_signal/wakeup (which schedule/acquire other locks),
        // so the whole exit_ptrace cannot be made atomic the way Linux's tasklist_lock allows. The EXITKILL verdict and send are
        // therefore transactionalized via the doom bit: phase one sets exitkill_pending in the same critical section that clears the
        // relation, and the right to send belongs to whichever consumer obtains the bit. Consuming here additionally requires still being
        // an orphan (not taken over by a concurrent attach) -- the doom consume and the relation check complete atomically in the same
        // critical section; if it has been re-attached, the consume right is left to the attach side (carry_out_pending_exitkill),
        // and this session no longer sends, closing the mistaken-kill window.
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
            // Only consume the doom bit while still an orphan: a re-attached tracee is taken over by the attach side.
            let doomed = orphan && exitkill && consume_exitkill_doom_locked(&tracee);
            (orphan, doomed)
        };
        if !still_orphan {
            // Already re-traced by a concurrent ATTACH: the new tracer owns this tracee, so skip this session's side effects.
            continue;
        }
        if doomed {
            // SIGKILL cannot be blocked/ignored; the tracee will be terminated
            let _ = Signal::SIGKILL.send_signal_info_to_pcb(None, tracee.clone(), PidType::PID);
        }

        if in_ptrace_stop {
            if group_stop_active && !exitkill {
                // group-stop is still active: keep Stopped and set CLD_STOPPED so real_parent's
                // wait can report it (CLD_STOPPED is a one-shot consumed bit that may already have been consumed
                // during the ptrace session). Skip on exitkill: the tracee is about to be SIGKILL-terminated and should no longer report a stop.
                tracee.sighand().flags_insert(SignalFlags::CLD_STOPPED);
            } else {
                // group-stop is no longer active, or the tracee is about to be SIGKILLed: unconditionally wake it out of ptrace_stop.
                let _ = ProcessManager::wakeup_stop(&tracee);
            }
            // real_parent's wait wakeup is independent of the tracee wakeup (notify if present).
            if let Some(real_parent) = real_parent {
                ProcessManager::wake_wait_parent(&real_parent);
            }
        } else if let Some(real_parent) = real_parent {
            // Not in a ptrace-stop (e.g. running): wake the waiters (parent + leader).
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
    // Fast path: the PTRACED bit is only written together with ptracer inside the relation-lock
    // critical section; if the bit is clear there is necessarily no tracer, so the global lock is unnecessary.
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
    // Fast path like ptracer_of: avoid the global lock when not traced.
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

/// Source of the caller's credentials used for access checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PtraceAccessCreds {
    /// Filesystem paths such as procfs: judged by fsuid/fsgid and the effective capability set
    FsCreds,
    /// Explicit syscalls: judged by the real uid/gid and the permitted capability set
    RealCreds,
}

// PCB ptrace methods -- relation establishment/teardown, attach/seize/detach, TRAPPING synchronization
impl ProcessControlBlock {
    pub fn has_permission_to_trace(&self, tracee: &Self, creds: PtraceAccessCreds) -> bool {
        // 1. The same thread group is always allowed access (introspection)
        if self.tgid == tracee.tgid {
            return true;
        }

        let caller_cred = self.cred();
        let tracee_cred = tracee.cred();
        let same_user_ns = Arc::ptr_eq(&caller_cred.user_ns, &tracee_cred.user_ns);
        let tracee_mm = tracee.basic().user_vm();
        // The caller's identity is selected according to the credential mode.
        let (caller_uid, caller_gid) = match creds {
            PtraceAccessCreds::FsCreds => (caller_cred.fsuid, caller_cred.fsgid),
            PtraceAccessCreds::RealCreds => (caller_cred.uid, caller_cred.gid),
        };
        // 2. Credential match + dumpable
        let uid_match = caller_uid == tracee_cred.euid
            && caller_uid == tracee_cred.suid
            && caller_uid == tracee_cred.uid;
        let gid_match = caller_gid == tracee_cred.egid
            && caller_gid == tracee_cred.sgid
            && caller_gid == tracee_cred.gid;
        // 3. CAP_SYS_PTRACE: the capability is evaluated in the target (tracee)'s user_ns,
        // not the caller's own ns, preventing a child user namespace from tracing a parent-ns process beyond its authority.
        let has_cap_in_task_ns = || {
            caller_cred.has_capability_in_ns(&tracee_cred.user_ns, cred::CAPFlags::CAP_SYS_PTRACE)
        };

        // Read-side barrier: pairs with the write-side barrier on the credential-commit path -- the write side
        // publishes dumpability first, then the new credentials; the read side inserts a barrier after reading the
        // tracee's credentials and before reading dumpable, so it never observes the "new credentials + old dumpable" window (attach at a privilege drop).
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

        // 4. Capability subset gate: the target's permitted set must be a subset of the caller's capability set (same user_ns).
        let caller_caps = match creds {
            PtraceAccessCreds::FsCreds => caller_cred.cap_effective,
            PtraceAccessCreds::RealCreds => caller_cred.cap_permitted,
        };
        (same_user_ns && (tracee_cred.cap_permitted.bits() & !caller_caps.bits()) == 0)
            || has_cap_in_task_ns()
    }

    /// Establish a tracing relation (called on the tracee side).
    /// The caller need not hold `PTRACE_RELATION_LOCK`; the function acquires it itself.
    pub fn ptrace_link(&self, tracer: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        if !tracer.has_permission_to_trace(self, PtraceAccessCreds::RealCreds) {
            return Err(SystemError::EPERM);
        }
        // Clear the ptrace options when establishing a new tracing relation, ensuring a re-attach does not
        // inherit the previous session's options. The SEIZE path overwrites them after link with the user-specified options; the ATTACH path keeps them empty.
        self.ptrace_link_locked(tracer, false)?;
        self.ptrace_state.lock_irqsave().options = PtraceOptions::empty();
        Ok(())
    }

    /// A fork/clone child automatically inherits the parent's tracing relation.
    /// Differences from ptrace_link: it does not re-check permissions; it skips when the tracer is
    /// exiting (returns Ok) so fork does not fail. The explicit attach path must still use ptrace_link for the permission check.
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

                // Refuse targets that are exiting or have already exited.
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

    /// Tear down the tracing relation and restore the tracee's execution state per its group-stop status:
    /// remove it from the ptraced list, clear the syscall-trace working bits, and either transition TracedStopped -> Stopped or wake it up.
    pub fn ptrace_unlink(&self) -> Result<(), SystemError> {
        let _relation_guard = PTRACE_RELATION_LOCK.lock_irqsave();

        // Take out the tracer and clear the bidirectional relation with an O(1) swap_remove.
        let me = self.self_ref.upgrade().ok_or(SystemError::ESRCH)?;
        let _tracer = unlink_relation_locked(&me);

        // Clear the syscall-trace / single-step working bits to avoid leftovers after detach.
        #[cfg(target_arch = "x86_64")]
        self.disable_single_step();

        self.flags().remove(
            ProcessFlags::TRACE_SYSCALL
                | ProcessFlags::TRACE_SINGLESTEP
                | ProcessFlags::TRACE_SYSEMU
                | ProcessFlags::PT_SEIZED,
        );
        // Decision: whether the tracee is currently really in a ptrace-stop (schedule has already set state=Stopped).
        let in_ptrace_stop = {
            let ps = self.ptrace_state.lock_irqsave();
            (ps.in_ptrace_stop || ps.listening) && self.sched_info().state().is_stopped()
        };
        let group_stop_active = self.sighand().flags_contains(SignalFlags::STOP_STOPPED);

        // Clear the ptrace-side state of this stop.
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
                    // group stop is still active: keep Stopped (the child was already Stopped)
                } else {
                    // group stop is no longer active: wake the tracee.
                    let _ = ProcessManager::wakeup_stop(&strong);
                }
            }
        }

        Ok(())
    }

    /// Whether this process is currently traced.
    pub fn is_traced(&self) -> bool {
        // Fast path: the PTRACED bit is only set/cleared inside the relation-lock critical section, so the global lock is unnecessary.
        if !self.flags().contains(ProcessFlags::PTRACED) {
            return false;
        }
        let _g = PTRACE_RELATION_LOCK.lock_irqsave();
        is_ptraced_locked(self)
    }

    /// Whether this process is currently traced by the given tracer.
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
            // Wake up the attach waiters
            self.wait_queue
                .wakeup_all(Some(ProcessState::Blocked(true)));
        }
    }

    /// The attach side waits for the tracee to complete the STOPPED -> TRACED transition (TRAPPING cleared).
    fn ptrace_wait_trapping_cleared(&self) {
        let _ = self.wait_queue.wait_event_killable(
            || !self.flags().contains(ProcessFlags::TRAPPING),
            None::<fn()>,
        );
    }

    /// If the tracee is currently in a group-stop (Stopped), queue an attach trap and wake it up to
    /// complete the STOPPED -> TRACED transition by itself. Aligns with JOBCTL_TRAP_STOP in Linux's ptrace_attach().
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
            // self_ref upgrade failed: the process is being destroyed, so roll back the relation.
            let _ = self.ptrace_unlink();
            SystemError::ESRCH
        })?;

        // Non-SEIZE ATTACH:
        // If the target is already in a group-stop (Stopped), convert its group-stop into a ptrace-stop
        // directly; only send SIGSTOP to make it stop when the target is not already Stopped.
        if self.ptrace_arm_attach_trap_if_stopped() {
            // The target is already group-stopped: wait for the tracee itself to commit the ptrace-stop and clear TRAPPING.
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
                // Roll back on attach failure: the target may carry an EXITKILL verdict left by the old
                // session (exit_ptrace's phase two was skipped because of this link), so take it over and
                // execute it here to avoid the doom bit being stranded and never consumed.
                let _ = self.ptrace_unlink();
                carry_out_pending_exitkill(self);
                return Err(e);
            }
        }
        // After the stop protocol completes, take over any EXITKILL verdict left by the old session (if present):
        // this corresponds to Linux attaching to an already SIGKILL-pending task -- attach succeeds, and the task
        // then dies. Placed here rather than inside link to avoid interleaving the TRAPPING wait with the death.
        carry_out_pending_exitkill(self);

        Ok(0)
    }

    /// Handle PTRACE_SEIZE.
    /// Does not send SIGSTOP; sets PT_SEIZED + options.
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
        // SUSPEND_SECCOMP requires CAP_SYS_ADMIN (DragonOS does not implement checkpoint-restore yet) and is
        // rejected consistently with the SETOPTIONS path, preventing unprivileged users from suspending the tracee's seccomp filtering.
        if options.contains(PtraceOptions::SUSPEND_SECCOMP) {
            return Err(SystemError::EPERM);
        }

        self.ptrace_link(tracer)?;
        self.flags().insert(ProcessFlags::PT_SEIZED);
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.options = options;
        }
        // Take over any EXITKILL verdict left by the old session (if present), same semantics as the tail of attach.
        carry_out_pending_exitkill(self);
        Ok(0)
    }

    /// Handle PTRACE_DETACH.
    pub fn ptrace_detach(&self, signal: Option<Signal>) -> Result<isize, SystemError> {
        let current_pcb = ProcessManager::current_pcb();
        if !self.is_traced_by(&current_pcb) {
            return Err(SystemError::EPERM);
        }

        // data=0 means no signal is injected
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
            // Do not clear in_ptrace_stop here so that ptrace_unlink can read it and wake the tracee correctly.
        }
        self.flags().remove(ProcessFlags::PTRACE_EVENT_STOP);

        self.ptrace_unlink()?;
        Ok(0)
    }

    /// Pre-check for ptrace operations: whether the tracee is traced by current and is in an operable state.
    pub fn ptrace_check_attach(&self, request: PtraceRequest) -> Result<(), SystemError> {
        let current = ProcessManager::current_pcb();
        if !self.is_traced_by(&current) {
            return Err(SystemError::ESRCH);
        }
        // KILL/INTERRUPT are allowed in any state
        if matches!(request, PtraceRequest::Kill | PtraceRequest::Interrupt) {
            return Ok(());
        }
        // Not operable in the LISTEN state
        // The remaining requests require the tracee to be in a ptrace-stop.
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

    /// Set the ptrace options (PTRACE_SETOPTIONS).
    pub fn set_ptrace_options(&self, options: PtraceOptions) -> Result<(), SystemError> {
        if options.contains(PtraceOptions::SUSPEND_SECCOMP) {
            return Err(SystemError::EPERM);
        }
        let mut ps = self.ptrace_state.lock_irqsave();
        ps.options = options;
        Ok(())
    }

    /// Read the most recent event message (PTRACE_GETEVENTMSG).
    pub fn ptrace_get_event_message(&self) -> usize {
        self.ptrace_state.lock_irqsave().stop_event_message()
    }

    /// Syscall stack accessor (ptrace needs to read the trap frame on the syscall stack).
    fn syscall_stack(&self) -> crate::libs::rwlock::RwLockReadGuard<'_, KernelStack> {
        self.syscall_stack.read()
    }

    /// Check whether the current rsp is within the syscall stack range.
    /// Used to dynamically determine which stack the TrapFrame is on in ptrace_stop.
    #[cfg(target_arch = "x86_64")]
    fn current_stop_frame_on_syscall_stack(&self) -> bool {
        let current_rsp = x86::current::registers::rsp() as usize;
        let syscall_stack = self.syscall_stack();
        let start = syscall_stack.start_address().data();
        let end = syscall_stack.stack_max_address().data();
        (start..end).contains(&current_rsp)
    }

    /// Compute the TrapFrame pointer on the kernel stack.
    fn trap_frame_ptr_on_kernel_stack(stack: &KernelStack) -> *mut TrapFrame {
        let ptr = stack.stack_max_address().data() - size_of::<TrapFrame>();
        ptr as *mut TrapFrame
    }

    /// Compute the TrapFrame pointer on the syscall stack.
    /// Note: init_syscall_stack sets GS:0x0 = stack_max - 8 (reserving 8 bytes),
    /// so the TrapFrame is actually at stack_max - 8 - sizeof(TrapFrame).
    #[cfg(target_arch = "x86_64")]
    fn trap_frame_ptr_on_syscall_stack(stack: &KernelStack) -> *mut TrapFrame {
        let ptr = stack.stack_max_address().data() - 8 - size_of::<TrapFrame>();
        ptr as *mut TrapFrame
    }

    /// Compute the TrapFrame pointer for the given stack choice.
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

    /// Re-verify under the ptrace_state lock that the tracee is still in a ptrace-stop (frame stable).
    /// A fatal signal may wake the tracee between check_attach and here; once the frame is stale,
    /// the caller should give up writing and return ESRCH.
    fn trap_frame_stable_locked(&self, ps: &PtraceState) -> bool {
        ps.in_ptrace_stop && self.sched_info().state().is_stopped()
    }

    /// Wait for the tracee to actually be scheduled out
    fn wait_tracee_descheduled(&self) {
        while self.sched_info().is_running() && self.sched_info().state().is_stopped() {
            core::hint::spin_loop();
        }
    }

    /// Read the tracee's user registers (PTRACE_GETREGS).
    #[cfg(target_arch = "x86_64")]
    pub fn tracee_user_regs(&self) -> Result<UserRegsStruct, SystemError> {
        loop {
            self.wait_tracee_descheduled();
            let ps = self.ptrace_state.lock_irqsave();
            if !Self::trap_frame_stable_locked(self, &ps) {
                return Err(SystemError::ESRCH);
            }
            if self.sched_info().is_running() {
                // After waiting, the tracee was woken up again and stopped once more in the pre-schedule window; retry.
                continue;
            }
            // SAFETY: The re-verification passed; the tracee is still in a ptrace-stop and the TrapFrame is stable.
            let frame = unsafe { &*self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            let mut regs = UserRegsStruct::from_trap_frame(frame);
            // fs/gs base are not in the TrapFrame: read them from the ArchPCBInfo authoritative storage (the latest values were read back at switch-out).
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
            // fs/gs base validation
            let user_end = MMArch::USER_END_VADDR.data() as u64;
            if regs.fs_base >= user_end || regs.gs_base >= user_end {
                return Err(SystemError::EIO);
            }
            // SAFETY: The tracee is still in a ptrace-stop and the TrapFrame is stable.
            let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            regs.write_to_trap_frame(frame)?;
            // Write fs/gs base into the ArchPCBInfo authoritative storage; only touches memory, not hardware,
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
        // 8-byte alignment check
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
        // General-purpose register area: offset 0..GP_REGS_SIZE
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
        // Debug register area: offset DR_OFFSET..=DR_OFFSET+56 (DR0-DR7, 8 slots)
        if (DR_OFFSET..DR_OFFSET + 8 * 8).contains(&offset) {
            let idx = (offset - DR_OFFSET) / 8;
            let mut val = self.ptrace_state.lock_irqsave().debug_regs[idx];
            // DR6 (slot6) stores the positive-polarity virtual_dr6; flip it back to the hardware polarity when returning.
            if idx == 6 {
                val ^= DR6_RESERVED;
            }
            return Ok(val as usize);
        }
        // Gap area (padding fields between GP_REGS_SIZE and DR_OFFSET): silently return 0.
        Ok(0)
    }

    /// PTRACE_POKEUSER: write one word at a byte offset.
    /// Requires 8-byte alignment and validation via putreg.
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
        // General-purpose register area: write back to the trap frame after putreg validation.
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
        // Debug register area.
        if (DR_OFFSET..DR_OFFSET + 8 * 8).contains(&offset) {
            let idx = (offset - DR_OFFSET) / 8;
            // DR4/DR5 do not exist; refuse the write.
            if idx == 4 || idx == 5 {
                return Err(SystemError::EIO);
            }

            let has_dr = {
                let mut ps = self.ptrace_state.lock_irqsave();
                match idx {
                    // DR0-3: address registers. Out-of-range values are forbidden
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
                    // DR6 (slot6) stores the positive-polarity virtual_dr6; flip when writing.
                    6 => {
                        ps.debug_regs[6] = val ^ DR6_RESERVED;
                    }
                    // DR7: control register. In the same critical section, "validate everything first, then commit
                    _ => {
                        let v = val & !DR_CONTROL_RESERVED;
                        for i in 0..4usize {
                            // A slot that is not enabled and whose address was never written has no
                            // combination to validate, so skip it; a slot whose address was written (a
                            // combination state exists) is validated regardless of whether it is enabled,
                            // guaranteeing the combination state is always valid under any write order.
                            if ((v >> (i * 2)) & 3) == 0 && ps.debug_regs[i] == 0 {
                                continue;
                            }
                            validate_dr_slot((v >> (16 + i * 4)) & 0xf, ps.debug_regs[i])?;
                        }
                        ps.debug_regs[7] = val;
                    }
                }
                // Maintain the hardware-breakpoint fast-path flag: any non-zero address register (DR0-3) or control register (DR7) counts as having configuration, and context switch loads/clears accordingly.
                ps.debug_regs[0..4].iter().any(|&v| v != 0) || ps.debug_regs[7] != 0
            };
            if has_dr {
                self.flags().insert(ProcessFlags::HW_DEBUG_REGS);
            } else {
                self.flags().remove(ProcessFlags::HW_DEBUG_REGS);
            }
            return Ok(());
        }
        // The gap area refuses writes.
        Err(SystemError::EIO)
    }

    /// Clear the hardware breakpoint configuration after a successful exec
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

    /// PTRACE_GETSIGINFO: read last_siginfo.
    pub fn ptrace_get_siginfo(&self) -> Result<SigInfo, SystemError> {
        let ps = self.ptrace_state.lock_irqsave();
        ps.stop_siginfo().ok_or(SystemError::EINVAL)
    }

    /// PTRACE_SETSIGINFO: write last_siginfo.
    pub fn ptrace_set_siginfo(&self, info: SigInfo) -> Result<(), SystemError> {
        let mut ps = self.ptrace_state.lock_irqsave();
        let slot = ps.stop_siginfo_mut().ok_or(SystemError::EINVAL)?;
        *slot = info;
        Ok(())
    }

    /// PTRACE_GETSIGMASK: read the current blocked mask.
    /// Returns a SigSet (DragonOS-v GenericSigSet, u64).
    pub fn ptrace_get_sigmask(&self) -> SigSet {
        let g = self.sig_info_irqsave();
        *g.sig_blocked()
    }

    /// PTRACE_SETSIGMASK: set the blocked mask (SIGKILL/SIGSTOP cannot be blocked).
    pub fn ptrace_set_sigmask(&self, mut new_set: SigSet) {
        new_set.remove(SigSet::SIGKILL);
        new_set.remove(SigSet::SIGSTOP);
        let mut g = self.sig_info_mut();
        *g.sig_block_mut() = new_set;
    }

    // PEEKDATA / POKEDATA -- read/write the tracee's user memory via the MM layer's unified remote-access API

    /// PTRACE_PEEKDATA/PEEKTEXT: read one word from the tracee's user space.
    /// Correctly handles page-crossing words (an 8-byte word spans two pages when addr is at the end of a page).
    pub fn ptrace_peek_data(&self, addr: usize) -> Result<usize, SystemError> {
        // Wrap-around prevention check
        let last = addr
            .checked_add(size_of::<usize>() - 1)
            .ok_or(SystemError::EIO)?;
        if last >= MMArch::USER_END_VADDR.data() {
            return Err(SystemError::EIO);
        }
        let _mm_guard = self.active_vm().ok_or(SystemError::ESRCH)?;
        let target_vm = _mm_guard.vm().clone();
        let mut bytes = [0u8; size_of::<usize>()];
        // Copy the whole word under a single AddressSpace read lock (force=true: ptrace override semantics)
        let n = target_vm.access_remote_vm(addr, RemoteAccess::Read(&mut bytes), true)?;
        if n != size_of::<usize>() {
            return Err(SystemError::EIO);
        }
        Ok(usize::from_ne_bytes(bytes))
    }

    /// PTRACE_POKEDATA/POKETEXT: write one word to the tracee's user space.
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

    /// PTRACE_GET_SYSCALL_INFO.
    /// Determines op from the mutable siginfo and event message published together with the stop
    /// generation, aligning with Linux 6.6's semantics of directly reading last_siginfo/ptrace_message.
    /// The op decision and frame read are completed in the same ptrace_state critical section, and the
    /// tracee is first re-verified to still be in a ptrace-stop (returning ESRCH on failure), so the two
    /// never come from snapshots assembled at different times; the user-space copy is done by the caller outside the lock.
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
        // On a Seccomp stop, ret_data = SECCOMP_RET_DATA
        let ret_data = if op == PtraceSyscallInfoOp::Seccomp {
            msg as u32
        } else {
            0
        };
        // Read the trap frame to fill in ip/sp/nr/args.
        // SAFETY: The re-verification passed; the tracee is still in a ptrace-stop and the TrapFrame is stable.
        let frame = unsafe { &*self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
        // AUDIT_ARCH_X86_64 (the only supported architecture on x86_64; switch to arch-provided when multi-arch).
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

    // Core stop state machine

    /// Request-level freeze of the tracee's ptrace-stop
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
            // SIGKILL is already pending: refuse the freeze; the tracee will take the death path,
            // which manifests as the usual ESRCH on the tracer side.
            return Err(SystemError::ESRCH);
        }
        {
            let mut ps = self.ptrace_state.lock_irqsave();
            // Re-verify the tracee is still in a ptrace-stop: a fatal wakeup may have already cleared the bit first.
            if !(ps.in_ptrace_stop && self.sched_info().state().is_stopped()) {
                return Err(SystemError::ESRCH);
            }
            ps.frozen = true;
        }
        Ok(())
        // The guards are released in the reverse order of their declaration: ptrace_state -> sig_info -> sighand inner.
    }

    /// Lift the request-level freeze and re-issue the deferred fatal wakeup.
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
                // Re-issue the death wakeup that was gated and deferred. wakeup_stop bails out early
                // for targets already Runnable (e.g. woken by CONT), so no double wakeup occurs.
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
    }

    /// Enter a ptrace-stop
    fn ptrace_stop(
        &self,
        exit_code: usize,
        why: SigChildCode,
        event_message: usize,
        info: Option<&mut SigInfo>,
    ) -> usize {
        // 1. Disable interrupts (released only right before schedule, keeping the check and commit atomic).
        let irq = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 2. Relation check + arm TRAPPING + commit Stopped must be in the same PTRACE_RELATION_LOCK critical section to close the detach race
        let relation_guard = super::PTRACE_RELATION_LOCK.lock_irqsave();
        if !super::ptrace::is_ptraced_locked(self) {
            drop(relation_guard);
            drop(irq);
            self.ptrace_clear_trapping();
            return exit_code;
        }

        // 3. Arm TRAPPING (if attach has set it).
        self.ptrace_set_trapping();

        // 4. Fatal check + commit TRACED state.
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
            // siginfo_g and sighand_g are only dropped after set_state
        };

        drop(relation_guard);

        // 5. fence(Release): ensures Stopped + exit_code are visible to the tracer before TRAPPING is cleared.
        fence(Ordering::Release);

        let Some(generation) = generation else {
            self.ptrace_clear_trapping();
            return 0;
        };

        // 6. Clear TRAPPING and wake the attach waiters.
        self.ptrace_clear_trapping();

        // group-stop participates in accounting + real_parent CLD_STOPPED notification.
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
        // 7. Notify the tracer and block.
        if let Some(tracer) = self.ptracer_pcb() {
            self.notify_tracer(&tracer, why, exit_code);
        }
        // real_parent CLD_STOPPED notification: only when group-stop completes && ptracer != real_parent.
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
                // Do not send CLD_STOPPED when real_parent has set SA_NOCLDSTOP or SIG_IGN
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
        // 8. Cleanup after wakeup.
        let mut ps = self.ptrace_state.lock_irqsave();
        let (saved_siginfo, injected) = ps.finish_waiter(generation);
        if let Some(i) = info {
            if let Some(saved) = saved_siginfo {
                // Refill: modifications from PTRACE_SETSIGINFO participate in subsequent signal delivery.
                *i = saved;
            }
        }
        // Only clean up the control bits if this generation is still the active stop; if another CPU
        // has already published a new-generation stop, the old waiter must not disturb the new stop's gating.
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

        // Recompute signal pending after wakeup (the tracer may have injected a signal).
        if let Some(strong) = self.self_ref.upgrade() {
            strong.recalc_sigpending();
        }

        result
    }

    /// Typed entry point for the group-stop path, so callers don't assemble the internal stop reason.
    pub(crate) fn ptrace_group_stop(&self, signal: Signal) -> usize {
        self.ptrace_stop(signal as usize, SigChildCode::Stopped, 0, None)
    }

    /// Consume a pending ptrace trap in the tracee context.
    /// Returns true if it has been handled; the caller should continue to re-verify the sticky pending bit.
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
            // A plain ATTACH group-stop from Linux's do_jobctl_trap() has no siginfo
            // and ignores the resume data.
            let _ = self.ptrace_group_stop(pending_sig);
        }
        true
    }

    /// Send SIGCHLD + wake the tracer's wait_queue.
    fn notify_tracer(
        &self,
        tracer: &Arc<ProcessControlBlock>,
        why: SigChildCode,
        stop_code: usize,
    ) {
        // 1. Send SIGCHLD to the tracer (if the tracer does not ignore it).
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
        // Unconditionally wake the ptracer's wait_queue.
        // gdb/strace do not install a SIGCHLD handler by default and block on waitpid(2); this wakeup is
        // their only reliable path to observe a ptrace-stop (the SIGCHLD above only serves signal-driven tracers).
        tracer.wake_all_waiters();
        // Also wake the group leader when it differs from the ptracer
        let leader = tracer
            .thread
            .read_irqsave()
            .group_leader()
            .unwrap_or_else(|| tracer.clone());
        if !Arc::ptr_eq(&leader, tracer) {
            leader.wake_all_waiters();
        }
    }

    /// ptrace event notification (FORK/CLONE/VFORK/EXEC/EXIT/SECCOMP).
    pub fn ptrace_event(&self, event: PtraceEvent, message: usize) {
        if self.ptrace_event_enabled(event) {
            let exit_code = (event as usize) << EXITCODE_EVENT_SHIFT | Signal::SIGTRAP as usize;
            // Only signal-delivery-stop and syscall-stop consume the injected signal returned by ptrace_notify;
            // FORK/CLONE/VFORK/EXEC/EXIT/SECCOMP events must not re-inject the signal the tracer passed via CONT back into the tracee.
            let _ = Self::ptrace_notify_with_message(exit_code, exit_code as i32, message);
        }
    }

    /// Check whether the event option is enabled.
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

    /// Construct PTRACE_EVENT_STOP (seize-mode group-stop / INTERRUPT / LISTEN re-trap).
    pub(crate) fn ptrace_event_stop(&self, signal: Signal) -> usize {
        let exit_code = (PtraceEvent::Stop as usize) << EXITCODE_EVENT_SHIFT | signal as usize;
        // si_code uses exit_code, so GETSIGINFO reads (Stop<<8)|signal.
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

    /// Called on the SIGCONT delivery path to make a seized tracee leave group-stop/LISTEN and re-enter PTRACE_EVENT_STOP.
    pub fn ptrace_trap_notify(&self) {
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return;
        }
        // Atomically write pending_event_stop and read the wakeup gate in a single lock hold:
        // only the LISTEN or group-stop (non-ptrace-stop) states need wakeup_stop to re-trap;
        // a normal ptrace-stop is left alone, with PENDING re-checked after CONT in do_signal_or_restart.
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

    /// PTRACE_INTERRUPT: make a running SEIZED tracee enter a ptrace-stop.
    pub fn ptrace_interrupt(&self) -> Result<(), SystemError> {
        // Requires PT_SEIZED
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return Err(SystemError::EIO);
        }
        // Atomically write pending_event_stop and read the wakeup gate in a single lock hold, avoiding a TOCTOU
        // between multiple lock acquisitions. Only a Stopped tracee in the LISTEN or group-stop (non-ptrace-stop)
        // state needs wakeup_stop to re-trap; a normal ptrace-stop is left alone, with PENDING re-checked after CONT in do_signal_or_restart.
        let wake_for_retrap = {
            let mut ps = self.ptrace_state.lock_irqsave();
            ps.pending_event_stop = Some(Signal::SIGTRAP);
            ps.listening || !ps.in_ptrace_stop
        };
        self.flags().insert(ProcessFlags::PENDING_PTRACE_STOP);
        if let Some(strong) = self.self_ref.upgrade() {
            // wakeup_stop is a no-op (idempotent) for non-Stopped processes; wakeup is a no-op for Stopped ones.
            // So a stale state read causes at most one redundant kick, and the sticky PENDING_PTRACE_STOP guarantees it eventually stops.
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

    /// PTRACE_LISTEN: make a tracee in PTRACE_EVENT_STOP leave its ptrace-stop but stay stopped.
    /// Semantics: the tracee neither runs (stays Stopped) nor is in a ptrace-stop (invisible to wait, ptrace
    /// commands fail with ESRCH); under a group-stop-originated LISTEN signals queue but are not delivered, and SIGCONT makes it leave the group-stop and re-trap.
    pub fn ptrace_listen(&self) -> Result<(), SystemError> {
        // PT_SEIZED
        if !self.flags().contains(ProcessFlags::PT_SEIZED) {
            return Err(SystemError::EIO);
        }
        let mut ps = self.ptrace_state.lock_irqsave();
        // LISTEN is only allowed on a PTRACE_EVENT_STOP trap
        let is_event_stop = ps
            .stop_siginfo()
            .map(|info| (info.sig_code().as_i32() >> 8) == PtraceEvent::Stop as i32)
            .unwrap_or(false);
        if !ps.in_ptrace_stop || !is_event_stop {
            return Err(SystemError::EIO);
        }
        // Extract the signal (low byte) that triggered this event-stop.
        let stop_signal = Signal::from(
            ps.current_stop
                .as_ref()
                .map(|stop| stop.exit_code)
                .unwrap_or(0) as i32
                & EXITCODE_SIG_MASK as i32,
        );
        let retrap = ps.pending_event_stop.is_some();
        ps.listening = true;
        // Clear in_ptrace_stop: in the LISTEN state the tracee is not in a ptrace-stop,
        // so ptrace_check_attach should return ESRCH
        ps.in_ptrace_stop = false;
        // Clear the request-level freeze in the same critical section
        ps.frozen = false;
        // Key: clear stop_report_pending so wait no longer reports this stop
        if let Some(stop) = ps.current_stop.as_mut() {
            stop.report_pending = false;
        }
        drop(ps);
        // group-stop origin (SIGSTOP/SIGTSTP/SIGTTIN/SIGTTOU): set STOP_STOPPED so the tracee keeps
        // group-stop semantics (signals queue but are not delivered; SIGCONT re-traps via ptrace_trap_notify).
        if stop_signal != Signal::INVALID
            && crate::ipc::signal_types::SIG_KERNEL_STOP_MASK.contains(stop_signal.into())
        {
            self.sighand().set_stop_signal(stop_signal);
            self.sighand().flags_insert(SignalFlags::STOP_STOPPED);
        }
        if retrap {
            // Wake the tracee out of ptrace_stop's schedule so it consumes PENDING_PTRACE_STOP on the
            // return-to-user path and re-enters PTRACE_EVENT_STOP.
            if let Some(strong) = self.self_ref.upgrade() {
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
        Ok(())
    }

    /// Send a ptrace notification stop (exit_code must be a SIGTRAP encoding).
    /// `si_code` is written into siginfo (e.g. TRAP_BRKPT/TRAP_TRACE/TRAP_HWBKPT, or an EVENT_STOP encoding).
    /// Generic trap entry with no event message.
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

    /// Re-inject the signal returned by ptrace_stop (if non-zero).
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

    /// Clear RFLAGS.TF that the debugger set for single-step
    #[cfg(target_arch = "x86_64")]
    fn disable_single_step(&self) {
        // Hold the lock throughout: clearing the flag, re-verification, and writing the frame are done in the same critical section
        let mut ps = self.ptrace_state.lock_irqsave();
        if !ps.forced_trap_flag {
            return;
        }
        ps.forced_trap_flag = false;
        // Only write frame.rflags while the tracee is still in a ptrace-stop (TrapFrame stable);
        // for a running tracee stop_frame_on_syscall_stack is a stale value, and writing it is both
        // ineffective and races with this CPU's entry path concurrently writing rflags.
        if Self::trap_frame_stable_locked(self, &ps) {
            // SAFETY: The re-verification passed; the frame is stable.
            let frame = unsafe { &mut *self.trap_frame_ptr_for(ps.stop_frame_on_syscall_stack) };
            frame.rflags &= !X86_EFLAGS_TF; // clear X86_EFLAGS_TF
        }
    }

    /// Set RFLAGS.TF after re-verifying under the ptrace_state lock that the tracee is still in a
    /// ptrace-stop, and record forced_trap_flag. A fatal signal may wake the tracee after the request
    /// is validated; once the stop frame is stale, refuse to write.
    #[cfg(target_arch = "x86_64")]
    fn arm_trap_flag_single_step(&self) -> Result<(), SystemError> {
        let mut ps = self.ptrace_state.lock_irqsave();
        if !Self::trap_frame_stable_locked(self, &ps) {
            return Err(SystemError::ESRCH);
        }
        // SAFETY: The re-verification passed; the frame is stable.
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

    /// Non-x86_64 architectures have no hardware single-step mechanism, so clearing single-step is a no-op.
    #[cfg(not(target_arch = "x86_64"))]
    #[inline]
    fn disable_single_step(&self) {}

    /// Resume the tracee (CONT/SYSCALL/SINGLESTEP/SYSEMU).
    /// Sets/clears the TRACE_* working bits, stores injected_signal, and wakes the tracee out of ptrace_stop.
    pub fn ptrace_resume(
        &self,
        request: PtraceRequest,
        signal: Option<Signal>,
    ) -> Result<isize, SystemError> {
        // Validate the injected signal first
        let resume_signal = match signal {
            None => Signal::INVALID,
            Some(Signal::INVALID) => return Err(SystemError::EIO),
            Some(s) => s,
        };

        // Set/clear the syscall-trace / single-step working bits.
        match request {
            PtraceRequest::Singlestep => {
                // Re-verify under the lock that the tracee is still in a ptrace-stop before setting
                // RFLAGS.TF: a fatal signal may have already woken it, invalidating the stop frame.
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
                    // SYSEMU_SINGLESTEP also needs hardware single-step armed (written after re-verification under the lock)
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

        // Store the injected signal, clear the stop flags, and wake the tracee.
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
                // Wake from Stopped back to Runnable
                let _ = ProcessManager::wakeup_stop(&strong);
            }
        }
        Ok(0)
    }
    /// ptrace-stop at syscall entry/exit (hot path).
    /// Returns true to indicate the syscall execution should be skipped (only meaningful for SYSEMU entry stops); false in all other cases.
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

    /// Slow path of ptrace_report_syscall: the tracee has syscall-trace / single-step enabled,
    /// so a ptrace-stop must be constructed and synchronized with the tracer. Only called when the hot-path early return misses.
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
        // Single-stepping across the syscall exit: report a single-step trap
        if !is_entry && is_single_step {
            if let Ok(signr) = Self::ptrace_notify(Signal::SIGTRAP as usize, TRAP_TRACE) {
                Self::reinject_ptrace_signal(signr);
            }
        } else {
            // Pure syscall-stop (entry or non-single-step exit): the sysgood bit is only added to a pure syscall-stop.
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
