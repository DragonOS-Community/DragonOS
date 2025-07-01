use core::fmt::Debug;

use crate::libs::rwlock::RwLock;
use crate::libs::spinlock::SpinLock;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use system_error::SystemError;

use super::namespace::pid_namespace::PidNamespace;
use super::ProcessControlBlock;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PidType {
    /// pid类型是进程id
    PID = 1,
    TGID = 2,
    PGID = 3,
    SID = 4,
    MAX = 5,
}

impl PidType {
    pub const PIDTYPE_MAX: usize = PidType::MAX as usize;
}

pub struct Pid {
    self_ref: Weak<Pid>,
    pub level: u32,
    /// 使用此PID的任务列表，按PID类型分组
    /// tasks[PidType::PID as usize] = 使用该PID作为进程ID的任务
    /// tasks[PidType::TGID as usize] = 使用该PID作为线程组ID的任务
    tasks: [SpinLock<Vec<Weak<ProcessControlBlock>>>; PidType::PIDTYPE_MAX],
    /// 在各个namespace中的PID值
    numbers: SpinLock<Vec<UPid>>,
}

impl Debug for Pid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Pid").finish()
    }
}

impl Pid {
    /// 获取指定PID所属的命名空间
    ///
    /// 返回该PID被分配时所在的PID命名空间的引用(Arc封装)
    pub fn ns_of_pid(&self) -> Arc<PidNamespace> {
        self.numbers
            .lock()
            .get(self.level as usize)
            .map(|upid| upid.ns.clone())
            .unwrap()
    }

    pub fn first_upid(&self) -> Option<UPid> {
        self.numbers.lock().first().cloned()
    }
}

/// 在特定namespace中的PID信息
#[derive(Clone)]
pub struct UPid {
    /// 在该namespace中的PID值
    pub nr: i32,
    /// 所属的namespace
    pub ns: Arc<PidNamespace>,
}

impl UPid {
    /// 创建新的UPid
    pub fn new(nr: i32, ns: Arc<PidNamespace>) -> Self {
        Self { nr, ns }
    }
}

impl Debug for UPid {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UPid").field("nr", &self.nr).finish()
    }
}

impl ProcessControlBlock {
    pub fn pid(&self) -> Option<Arc<Pid>> {
        self.thread_pid.read().clone()
    }
}

/// 连接任务和PID的桥梁结构体
#[derive(Debug)]
pub struct PidLink {
    /// 指向对应的Pid结构体
    pub pid: RwLock<Option<Arc<Pid>>>,
}

impl PidLink {
    /// 创建新的PidLink
    pub fn new() -> Self {
        Self {
            pid: RwLock::new(None),
        }
    }

    /// 链接到指定的PID
    pub fn link_pid(&self, pid: Arc<Pid>) {
        self.pid.write().replace(pid);
    }

    /// 取消PID链接
    pub fn unlink_pid(&self) {
        self.pid.write().take();
    }

    /// 获取链接的PID
    pub fn get_pid(&self) -> Option<Arc<Pid>> {
        self.pid.read().clone()
    }

    /// 检查是否已链接PID
    pub fn is_linked(&self) -> bool {
        self.pid.read().is_some()
    }
}

impl Default for PidLink {
    fn default() -> Self {
        Self::new()
    }
}

impl Clone for PidLink {
    fn clone(&self) -> Self {
        Self {
            pid: RwLock::new(self.get_pid()),
        }
    }
}

/// 分配一个新的PID
///
/// 参考：https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/pid.c?fi=alloc_pid#162
pub(super) fn alloc_pid(ns: &Arc<PidNamespace>) -> Result<Arc<Pid>, SystemError> {
    todo!("alloc_pid not implemented yet");
}
