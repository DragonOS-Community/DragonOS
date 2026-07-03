use core::{fmt, str::FromStr};

use super::RawPid;

impl fmt::Display for RawPid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for RawPid {
    type Err = core::num::ParseIntError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let pid = usize::from_str(s)?;
        Ok(RawPid(pid))
    }
}

impl RawPid {
    /// This RawPid has not been assigned yet and will be initialized later.
    /// This state should only appear during process/thread creation.
    pub const UNASSIGNED: RawPid = RawPid(usize::MAX - 1);
    pub const MAX_VALID: RawPid = RawPid(usize::MAX - 32);

    pub fn is_valid(&self) -> bool {
        self.0 >= Self::MAX_VALID.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessState {
    /// The process is running on a CPU or in a run queue.
    Runnable,
    /// The process is waiting for an event to occur.
    /// The bool indicates whether the wait is interruptible.
    /// - If true, hardware interrupts, signals, and other system events can
    ///   interrupt the wait and transition the process back to Runnable.
    /// - If false, the process must be explicitly woken up to return to Runnable.
    Blocked(bool),
    /// The process was stopped by a signal.
    Stopped,
    /// The process has exited; usize holds the raw wait status used by Linux wait(2) family.
    ///
    /// Normal exit: `(exit_code & 0xff) << 8`; signal termination: signal number in the low 7 bits.
    /// wait4/waitpid return this value as-is; only waitid `si_status` needs decoding from it.
    Exited(usize),
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExitState {
    Running = 0,
    Zombie = 1,
    Dead = 2,
}

impl ExitState {
    pub(super) fn from_u8(v: u8) -> Self {
        match v {
            1 => ExitState::Zombie,
            2 => ExitState::Dead,
            _ => ExitState::Running,
        }
    }
}

mod state_bits {
    pub const TASK_RUNNING: u32 = 0x0000;
    pub const TASK_INTERRUPTIBLE: u32 = 0x0001;
    pub const TASK_UNINTERRUPTIBLE: u32 = 0x0002;
    pub const TASK_STOPPED: u32 = 0x0004;
    pub const TASK_DEAD_MARKER: u32 = 0x0080;
    pub const EXIT_CODE_SHIFT: u32 = 12;
}

#[allow(dead_code)]
impl ProcessState {
    #[inline(always)]
    pub fn is_runnable(&self) -> bool {
        return matches!(self, ProcessState::Runnable);
    }

    #[inline(always)]
    pub fn is_blocked(&self) -> bool {
        return matches!(self, ProcessState::Blocked(_));
    }

    #[inline(always)]
    pub fn is_blocked_interruptable(&self) -> bool {
        return matches!(self, ProcessState::Blocked(true));
    }

    /// Returns `true` if the process state is [`Exited`].
    #[inline(always)]
    pub fn is_exited(&self) -> bool {
        return matches!(self, ProcessState::Exited(_));
    }

    /// Returns `true` if the process state is [`Stopped`].
    ///
    /// [`Stopped`]: ProcessState::Stopped
    #[inline(always)]
    pub fn is_stopped(&self) -> bool {
        matches!(self, ProcessState::Stopped)
    }

    /// Returns raw wait status if the process state is [`Exited`].
    #[inline(always)]
    pub fn raw_wstatus(&self) -> Option<usize> {
        match self {
            ProcessState::Exited(code) => Some(*code),
            _ => None,
        }
    }

    /// Returns raw wait status if the process state is [`Exited`].
    ///
    /// Kept for existing call sites; new wait code should prefer
    /// [`ProcessState::raw_wstatus`] to avoid confusing raw wait status with
    /// the user-visible exit code.
    #[inline(always)]
    pub fn exit_code(&self) -> Option<usize> {
        self.raw_wstatus()
    }

    #[inline]
    pub fn to_u32(self) -> u32 {
        match self {
            ProcessState::Runnable => state_bits::TASK_RUNNING,
            ProcessState::Blocked(true) => state_bits::TASK_INTERRUPTIBLE,
            ProcessState::Blocked(false) => state_bits::TASK_UNINTERRUPTIBLE,
            ProcessState::Stopped => state_bits::TASK_STOPPED,
            ProcessState::Exited(code) => {
                state_bits::TASK_DEAD_MARKER | ((code as u32) << state_bits::EXIT_CODE_SHIFT)
            }
        }
    }

    #[inline]
    pub fn from_u32(val: u32) -> Self {
        if val & state_bits::TASK_DEAD_MARKER != 0 {
            let code = (val >> state_bits::EXIT_CODE_SHIFT) as usize;
            ProcessState::Exited(code)
        } else {
            match val {
                v if v == state_bits::TASK_RUNNING => ProcessState::Runnable,
                v if v == state_bits::TASK_INTERRUPTIBLE => ProcessState::Blocked(true),
                v if v == state_bits::TASK_UNINTERRUPTIBLE => ProcessState::Blocked(false),
                v if v == state_bits::TASK_STOPPED => ProcessState::Stopped,
                _ => {
                    panic!(
                        "ProcessState::from_u32: corrupted state value 0x{val:08x}, \
                         this indicates memory corruption or a kernel bug"
                    );
                }
            }
        }
    }
}

bitflags! {
    /// Process control block flags.
    pub struct ProcessFlags: usize {
        /// This PCB represents a kernel thread.
        const KTHREAD = 1 << 0;
        /// This process needs to be scheduled.
        const NEED_SCHEDULE = 1 << 1;
        /// Process shares resources with its parent due to vfork.
        const VFORK = 1 << 2;
        /// Process cannot be frozen.
        const NOFREEZE = 1 << 3;
        /// Process is exiting.
        const EXITING = 1 << 4;
        /// Process was woken up by a terminating signal.
        const WAKEKILL = 1 << 5;
        /// Process exited due to receiving a signal (killed by a signal).
        const SIGNALED = 1 << 6;
        /// Process needs to be migrated to another CPU.
        const NEED_MIGRATE = 1 << 7;
        /// Randomized virtual address space; primarily used for dynamic linker
        /// loading.
        const RANDOMIZE = 1 << 8;
        /// Process has a pending signal (a fast-check flag).
        /// Equivalent to Linux's TIF_SIGPENDING.
        const HAS_PENDING_SIGNAL = 1 << 9;
        /// Process needs to restore a previously saved signal mask.
        const RESTORE_SIG_MASK = 1 << 10;
        /// Forked but didn't exec.
        const FORKNOEXEC = 1 << 11;
        /// Process needs to handle rseq before returning to userspace.
        const NEED_RSEQ = 1 << 12;
        /// Process is waiting for an I/O operation to complete (used for iowait
        /// accounting).
        const IN_IOWAIT = 1 << 13;
        /// Delay unhash of PID/TGID/PGID/SID during thread-group exec.
        const DEFER_UNHASH = 1 << 14;
        /// PID links and visible-thread accounting have already been released.
        const PID_UNHASHED = 1 << 15;
        /// Task is currently traced by another task.
        const PTRACED = 1 << 16;
    }
}

impl ProcessFlags {
    pub const fn fork_inherited(&self) -> Self {
        Self::from_bits_truncate(self.bits & Self::RANDOMIZE.bits)
    }

    pub const fn exit_to_user_mode_work(&self) -> Self {
        Self::from_bits_truncate(
            self.bits
                & (Self::NEED_SCHEDULE.bits | Self::HAS_PENDING_SIGNAL.bits | Self::NEED_RSEQ.bits),
        )
    }

    /// Test and clear flags.
    ///
    /// ## Parameters
    ///
    /// - `rhs`: The flags to test and clear.
    ///
    /// ## Returns
    ///
    /// `true` if the flags were set before clearing, otherwise `false`.
    pub const fn test_and_clear(&mut self, rhs: Self) -> bool {
        let r = (self.bits & rhs.bits) != 0;
        self.bits &= !rhs.bits;
        r
    }
}
