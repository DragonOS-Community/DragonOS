use num_traits::FromPrimitive;
use system_error::SystemError;

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use crate::time::syscall::PosixTimeval;

use super::{ProcessControlBlock, ProcessManager};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct RUsage {
    /// User time used
    pub ru_utime: PosixTimeval,
    /// System time used
    pub ru_stime: PosixTimeval,

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

impl RUsage {
    #[inline]
    fn add_time(lhs: &mut PosixTimeval, rhs: PosixTimeval) {
        *lhs = PosixTimeval::from_ns(lhs.to_ns().saturating_add(rhs.to_ns()));
    }

    pub fn add_assign_saturating(&mut self, rhs: &RUsage) {
        Self::add_time(&mut self.ru_utime, rhs.ru_utime);
        Self::add_time(&mut self.ru_stime, rhs.ru_stime);
        self.ru_maxrss = self.ru_maxrss.max(rhs.ru_maxrss);
        self.ru_ixrss = self.ru_ixrss.saturating_add(rhs.ru_ixrss);
        self.ru_idrss = self.ru_idrss.saturating_add(rhs.ru_idrss);
        self.ru_isrss = self.ru_isrss.saturating_add(rhs.ru_isrss);
        self.ru_minflt = self.ru_minflt.saturating_add(rhs.ru_minflt);
        self.ru_majflt = self.ru_majflt.saturating_add(rhs.ru_majflt);
        self.ru_nswap = self.ru_nswap.saturating_add(rhs.ru_nswap);
        self.ru_inblock = self.ru_inblock.saturating_add(rhs.ru_inblock);
        self.ru_oublock = self.ru_oublock.saturating_add(rhs.ru_oublock);
        self.ru_msgsnd = self.ru_msgsnd.saturating_add(rhs.ru_msgsnd);
        self.ru_msgrcv = self.ru_msgrcv.saturating_add(rhs.ru_msgrcv);
        self.ru_nsignals = self.ru_nsignals.saturating_add(rhs.ru_nsignals);
        self.ru_nvcsw = self.ru_nvcsw.saturating_add(rhs.ru_nvcsw);
        self.ru_nivcsw = self.ru_nivcsw.saturating_add(rhs.ru_nivcsw);
    }
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
    fn leader_for_rusage(&self) -> Arc<ProcessControlBlock> {
        if self.is_thread_group_leader() {
            return self
                .self_ref
                .upgrade()
                .unwrap_or_else(ProcessManager::current_pcb);
        }

        self.threads_read_irqsave()
            .group_leader()
            .or_else(|| self.self_ref.upgrade())
            .unwrap_or_else(ProcessManager::current_pcb)
    }

    fn task_rusage(&self) -> RUsage {
        let ct = self.cputime();
        RUsage {
            ru_utime: PosixTimeval::from_ns(ct.utime.load(Ordering::Relaxed)),
            ru_stime: PosixTimeval::from_ns(ct.stime.load(Ordering::Relaxed)),
            ..RUsage::default()
        }
    }

    fn thread_group_rusage(&self) -> RUsage {
        let leader = self.leader_for_rusage();
        let mut usage = leader.task_rusage();
        let ti = leader.threads_read_irqsave();
        for task in &ti.group_tasks {
            if let Some(task) = task.upgrade() {
                usage.add_assign_saturating(&task.task_rusage());
            }
        }
        usage
    }

    pub fn add_child_rusage(&self, rusage: &RUsage) {
        let leader = self.leader_for_rusage();
        leader.children_rusage.lock().add_assign_saturating(rusage);
    }

    /// 获取进程资源使用情况
    pub fn get_rusage(&self, who: RUsageWho) -> Option<RUsage> {
        match who {
            RUsageWho::RUsageSelf => Some(self.thread_group_rusage()),
            RUsageWho::RUsageBoth => {
                let mut rusage = self.thread_group_rusage();
                let leader = self.leader_for_rusage();
                let children = *leader.children_rusage.lock();
                rusage.add_assign_saturating(&children);
                Some(rusage)
            }
            RUsageWho::RusageThread => Some(self.task_rusage()),
            RUsageWho::RUsageChildren => {
                let leader = self.leader_for_rusage();
                let rusage = *leader.children_rusage.lock();
                Some(rusage)
            }
        }
    }
}
