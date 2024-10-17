//! # 网络模块
//! 注意，net模块下，为了方便导入，模块细分，且共用部分模块直接使用
//! `pub use`导出，导入时也常见`use crate::net::socket::*`的写法，
//! 敬请注意。
use core::sync::atomic::AtomicUsize;

use alloc::{collections::BTreeMap, sync::Arc};

use crate::{driver::net::Iface, libs::rwlock::RwLock};

pub mod event_poll;
pub mod net_core;
pub mod posix;
pub mod socket;
pub mod syscall;

lazy_static! {
    /// # 所有网络接口的列表
    /// 这个列表在中断上下文会使用到，因此需要irqsave
    pub static ref NET_DEVICES: RwLock<BTreeMap<usize, Arc<dyn Iface>>> = RwLock::new(BTreeMap::new());
}

/// 生成网络接口的id (全局自增)
pub fn generate_iface_id() -> usize {
    static IFACE_ID: AtomicUsize = AtomicUsize::new(0);
    return IFACE_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}
