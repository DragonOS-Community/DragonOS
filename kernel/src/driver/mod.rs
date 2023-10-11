pub mod acpi;
pub mod base;
pub mod disk;
pub mod keyboard;
pub mod net;
pub mod pci;
pub mod timers;
pub mod tty;
pub mod video;
pub mod virtio;

use core::fmt::Debug;

use alloc::{sync::Arc, vec::Vec};

use crate::syscall::SystemError;

use self::base::{
    device::{driver::DriverError, Device, DevicePrivateData, DeviceResource, IdTable},
    kobject::KObject,
    platform::CompatibleTable,
};
pub trait Driver: Sync + Send + Debug + KObject {
    fn probe(&self, device: &Arc<dyn Device>) -> Result<(), SystemError>;
    fn remove(&self, device: &Arc<dyn Device>) -> Result<(), SystemError>;
    fn sync_state(&self, device: &Arc<dyn Device>);
    fn shutdown(&self, device: &Arc<dyn Device>);
    fn suspend(&self, device: &Arc<dyn Device>) {
        // todo: implement suspend
    }

    fn resume(&self, device: &Arc<dyn Device>) -> Result<(), SystemError>;

    fn coredump(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    /// @brief: 获取驱动标识符
    /// @parameter: None
    /// @return: 该驱动驱动唯一标识符
    fn id_table(&self) -> IdTable;

    fn devices(&self) -> Vec<Arc<dyn Device>>;

    /// 是否禁用sysfs的bind/unbind属性
    ///
    /// ## 返回
    ///
    /// - true: 禁用
    /// - false: 不禁用（默认）
    fn suppress_bind_attrs(&self) -> bool {
        false
    }
}
