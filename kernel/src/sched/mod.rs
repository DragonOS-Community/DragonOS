pub mod cfs;
pub mod completion;
pub mod core;
pub mod rt;
pub mod syscall;

/// 调度策略
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

impl Into<i32> for SchedPriority {
    fn into(self) -> i32 {
        self.0
    }
}
