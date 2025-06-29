pub mod nsproxy;
pub mod pid_namespace;

/// Namespace 种类，用于运行时调试与 /proc 导出
#[repr(u8)]
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

/// 每个具体 Namespace 内部都嵌入一份公共字段
#[derive(Debug)]
pub struct NamespaceBase {
    /// 层级（root = 0）
    level: u32,
    /// 种类
    ty: NamespaceType,
}

impl NamespaceBase {
    pub fn new(level: u32, ty: NamespaceType) -> Self {
        Self { level, ty }
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    pub fn ty(&self) -> NamespaceType {
        self.ty
    }
}

/// Namespace 通用操作接口
pub trait NamespaceOps: Send + Sync {
    /// 返回公共字段，便于统一处理
    fn base(&self) -> &NamespaceBase;

    /// 用于 debug /proc 导出
    fn ty(&self) -> NamespaceType {
        self.base().ty()
    }

    /// 当最后一个 Arc 引用被丢弃时调用，用于资源清理
    fn cleanup(&self);
}
