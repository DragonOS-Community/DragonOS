pub mod cfs;
pub mod completion;
pub mod core;
pub mod rt;
pub mod syscall;

/// 调度策略
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedPolicy {
    /// 完全公平调度
    CFS,
    /// 先进先出调度
    FIFO,
    /// 轮转调度
    RR,
}

/// 调度优先级
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct SchedPriority(i32);

impl SchedPriority {
    const MIN: i32 = 0;
    const MAX: i32 = 139;

    /// 创建一个新的调度优先级
    pub const fn new(priority: i32) -> Option<Self> {
        if Self::validate(priority) {
            Some(Self(priority))
        } else {
            None
        }
    }

    /// 校验优先级是否合法
    pub const fn validate(priority: i32) -> bool {
        priority >= Self::MIN && priority <= Self::MAX
    }

    pub fn data(&self) -> i32 {
        self.0
    }
}

pub trait SchedArch {
    /// 开启当前核心的调度
    fn enable_sched_local();
    /// 关闭当前核心的调度
    fn disable_sched_local();

    /// 在第一次开启调度之前，进行初始化工作。
    ///
    /// 注意区别于sched_init，这个函数只是做初始化时钟的工作等等。
    fn initial_setup_sched_local() {}
}
