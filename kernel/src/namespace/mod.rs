use core::sync::atomic::AtomicUsize;

use alloc::sync::Arc;
use mnt_namespace::MntNamespace;
use pid_namespace::PidNamespace;

pub mod mnt_namespace;
pub mod namespace;
pub mod pid_namespace;
pub mod ucount;
pub mod user_namespace;

/// 管理 namespace,包含了所有namespace的信息
pub struct NsSet {
    flags: u32,
    nsproxy: Arc<NsProxy>,
}

pub struct NsProxy {
    pub count: AtomicUsize,
    pub pid_namespace: Arc<PidNamespace>,
    pub mnt_namespace: Arc<MntNamespace>,
}

pub struct FsStruct {
    users: u32, // 用户个数
    umask: u32, // 用户掩码
    in_exec: u32,
}
#[macro_export]
macro_rules! container_of {
    ($ptr:expr, $struct:path, $field:ident) => {
        unsafe {
            let dummy = core::mem::MaybeUninit::<$struct>::uninit();
            let dummy_ptr = dummy.as_ptr();
            let field_ptr = &(*dummy_ptr).$field as *const _ as usize;
            let offset = field_ptr - dummy_ptr as usize;
            ($ptr as *const u8).wrapping_sub(offset) as *mut $struct
        }
    };
}
