pub mod nsproxy;
pub mod pid_namespace;
pub mod user_namespace;

use nsproxy::NsCommon;

/// Namespace 种类，用于运行时调试与 /proc 导出
#[repr(u8)]
#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NamespaceType {
    Pid,
    Mount,
    Net,
    Ipc,
    Uts,
    User,
    Cgroup,
    Time,
}

/// Namespace 通用操作接口
#[allow(dead_code)]
pub trait NamespaceOps: Send + Sync {
    /// 返回公共字段，便于统一处理
    fn ns_common(&self) -> &NsCommon;

    /// 用于 debug /proc 导出
    fn ty(&self) -> NamespaceType {
        self.ns_common().ty()
    }

    /// 获取层级
    fn level(&self) -> u32 {
        self.ns_common().level()
    }
}
