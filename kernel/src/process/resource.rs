use crate::{syscall::SystemError, time::TimeSpec};

use super::ProcessControlBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RUsage {
    /// User time used
    pub ru_utime: TimeSpec,
    /// System time used
    pub ru_stime: TimeSpec,

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
