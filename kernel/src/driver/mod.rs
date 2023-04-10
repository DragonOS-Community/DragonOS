use core::{sync::atomic::AtomicUsize, any::Any, fmt::Debug};

use alloc::{collections::BTreeMap, sync::Arc};

use crate::libs::rwlock::RwLock;
use net::NetDriver;

pub mod acpi;
pub mod base;
pub mod disk;
pub mod keyboard;
pub mod net;
pub mod pci;
pub mod timers;
pub mod tty;
pub mod uart;
pub mod video;
pub mod virtio;

lazy_static! {
    /// @brief 所有的网卡驱动的列表
    /// key: 网卡的id
    /// value: 网卡的驱动
    pub static ref NET_DRIVERS: RwLock<BTreeMap<usize, Arc<dyn NetDriver>>> = RwLock::new(BTreeMap::new());
}

/// @brief 生成网卡的id
pub fn generate_nic_id() -> usize {
    static NET_ID: AtomicUsize = AtomicUsize::new(0);
    return NET_ID
        .fetch_add(1, core::sync::atomic::Ordering::SeqCst)
        .into();
}

pub trait Driver: Sync + Send + Debug{
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any;

    fn as_any_mut(&'static mut self) -> &'static mut dyn core::any::Any;

}

