use core::sync::atomic::AtomicUsize;

use crate::namespace::pid_namespace::PidNamespace;
use alloc::sync::Arc;

pub mod namespace;
pub mod pid_namespace;
pub mod ucount;
pub mod user_namespace;

/// 管理 namespace
struct NsSet {
    flags: u32,
    ns_proxy: Arc<NsProxy>,
}

struct NsProxy {
    count: AtomicUsize,
    pid_namespace: Arc<PidNamespace>,
    // 需要什么namespace下次加入
}

struct FsStruct {
    users: u32, // 用户个数
    umask: u32, // 用户掩码
    in_exec: u32,
}
