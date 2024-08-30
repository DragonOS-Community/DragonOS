use core::sync::atomic::AtomicUsize;

use alloc::sync::Arc;
use mnt_namespace::{FsStruct, MntNamespace};
use pid_namespace::PidNamespace;

use crate::libs::spinlock::SpinLock;

pub mod mnt_namespace;
pub mod namespace;
pub mod pid_namespace;
pub mod ucount;
pub mod user_namespace;

/// 管理 namespace,包含了所有namespace的信息
pub struct NsSet {
    flags: u32,
    nsproxy: NsProxy,
    fs: Arc<SpinLock<FsStruct>>,
}
#[derive(Debug)]
pub struct NsProxy {
    pub count: AtomicUsize,
    pub pid_namespace: Option<Arc<PidNamespace>>,
    pub mnt_namespace: Option<Arc<MntNamespace>>,
}

impl NsProxy {
    pub fn new() -> Self {
        Self {
            count: AtomicUsize::new(1),
            pid_namespace: None,
            mnt_namespace: None,
        }
    }
}

#[macro_export]
macro_rules! container_of {
    ($ptr:expr, $struct:path, $field:ident) => {
        unsafe {
            let dummy = core::mem::MaybeUninit::<$struct>::uninit();
            let dummy_ptr = dummy.as_ptr();
            let field_ptr = &(*dummy_ptr).$field as *const _ as usize;
            let offset = field_ptr - dummy_ptr as usize;
            Arc::from_raw(($ptr as *const u8).wrapping_sub(offset) as *mut $struct)
        }
    };
}
