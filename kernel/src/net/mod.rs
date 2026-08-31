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
mod rtnl;
pub mod socket;
pub mod syscall;
pub mod tcp_close_defer;
pub mod tcp_listener_backlog;

/// Linux reserves interface index 1 for the loopback device in every network namespace.
pub const LOOPBACK_IFINDEX: usize = 1;

/// 生成网络接口的id (全局自增)
pub fn generate_iface_id() -> usize {
    // Linux reserves ifindex 1 for the loopback device in each network
    // namespace. Root-namespace devices use this global allocator, so start
    // dynamic allocation at 2 regardless of initcall/link order.
    static IFACE_ID: AtomicUsize = AtomicUsize::new(LOOPBACK_IFINDEX + 1);
    return IFACE_ID.fetch_add(1, core::sync::atomic::Ordering::SeqCst);
}
