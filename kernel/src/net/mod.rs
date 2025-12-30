//! # 网络模块
//! 注意，net模块下，为了方便导入，模块细分，且共用部分模块直接使用
//! `pub use`导出，导入时也常见`use crate::net::socket::*`的写法，
//! 敬请注意。
use core::sync::atomic::AtomicUsize;

use crate::driver::net::Iface;

pub mod neighbor;
pub mod net_core;
pub mod posix;
pub mod routing;
pub mod socket;
pub mod syscall;

/// 生成网络接口的id (全局自增)
pub fn generate_iface_id() -> usize {
    static IFACE_ID: AtomicUsize = AtomicUsize::new(0);
    return IFACE_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}
