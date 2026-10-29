use system_error::SystemError;

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

/// ELF NT_PRSTATUS note type (used by PTRACE_GETREGSET/SETREGSET).
pub const NT_PRSTATUS: u32 = 1;
