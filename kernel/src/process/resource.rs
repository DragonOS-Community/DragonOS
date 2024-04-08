use num_traits::FromPrimitive;
use system_error::SystemError;

use crate::time::PosixTimeSpec;

use super::ProcessControlBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RUsage {
    /// User time used
    pub ru_utime: PosixTimeSpec,
    /// System time used
    pub ru_stime: PosixTimeSpec,

    // 以下是linux的rusage结构体扩展
    /// Maximum resident set size
    pub ru_maxrss: usize,
    /// Integral shared memory size
    pub ru_ixrss: usize,
    /// Integral unshared data size
    pub ru_idrss: usize,
    /// Integral unshared stack size
    pub ru_isrss: usize,
    /// Page reclaims (soft page faults)
    pub ru_minflt: usize,
    /// Page faults (hard page faults)
    pub ru_majflt: usize,
    /// Swaps
    pub ru_nswap: usize,
    /// Block input operations
    pub ru_inblock: usize,
    /// Block output operations
    pub ru_oublock: usize,
    /// IPC messages sent
    pub ru_msgsnd: usize,
    /// IPC messages received
    pub ru_msgrcv: usize,
    /// Signals received
    pub ru_nsignals: usize,
    /// Voluntary context switches
    pub ru_nvcsw: usize,
    /// Involuntary context switches
    pub ru_nivcsw: usize,
}

///
///  Definition of struct rusage taken from BSD 4.3 Reno
///
///  We don't support all of these yet, but we might as well have them....
///  Otherwise, each time we add new items, programs which depend on this
///  structure will lose.  This reduces the chances of that happening.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RUsageWho {
    RUsageSelf = 0,
    RUsageChildren = -1,
    /// sys_wait4() uses this
    RUsageBoth = -2,
    /// only the calling thread
    RusageThread = 1,
}

impl TryFrom<i32> for RUsageWho {
    type Error = SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(RUsageWho::RUsageSelf),
            -1 => Ok(RUsageWho::RUsageChildren),
            -2 => Ok(RUsageWho::RUsageBoth),
            1 => Ok(RUsageWho::RusageThread),
            _ => Err(SystemError::EINVAL),
        }
    }
}

/// Resource limit
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub struct RLimit64 {
    /// The current (soft) limit
    pub rlim_cur: u64,
    /// The hard limit
    pub rlim_max: u64,
}

/// Resource limit IDs
///
/// ## Note
///
/// 有些架构中，这里[5,9]的值是不同的，我们将来需要在这里增加条件编译
#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive)]
pub enum RLimitID {
    /// CPU time in sec
    Cpu = 0,
    /// Maximum file size
    Fsize = 1,
    /// Max data size
    Data = 2,
    /// Max stack size
    Stack = 3,
    /// Max core file size
    Core = 4,
    /// Max resident set size
    Rss = 5,

    /// Max number of processes
    Nproc = 6,
    /// Max number of open files
    Nofile = 7,
    /// Max locked-in-memory address space
    Memlock = 8,
    /// Address space limit
    As = 9,
    /// Max number of file locks held
    Locks = 10,

    /// Max number of pending signals
    Sigpending = 11,
    /// Max bytes in POSIX mqueues
    Msgqueue = 12,
    /// Max nice prio allowed to raise to
    ///  0-39 for nice level 19 .. -20
    Nice = 13,
    /// Max realtime priority
    Rtprio = 14,
    /// Timeout for RT tasks in us
    Rttime = 15,
    Nlimits = 16,
}

impl TryFrom<usize> for RLimitID {
    type Error = SystemError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        <Self as FromPrimitive>::from_usize(value).ok_or(SystemError::EINVAL)
    }
}

impl ProcessControlBlock {
    /// 获取进程资源使用情况
    ///
    /// ## TODO
    ///
    /// 当前函数尚未实现，只是返回了一个默认的RUsage结构体
    pub fn get_rusage(&self, _who: RUsageWho) -> Option<RUsage> {
        let rusage = RUsage::default();

        Some(rusage)
    }
}
