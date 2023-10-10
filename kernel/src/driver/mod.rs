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

use core::fmt::Debug;

use alloc::sync::Arc;

use self::base::{
    device::{driver::DriverError, Device, DevicePrivateData, DeviceResource, IdTable},
    kobject::KObject,
    platform::CompatibleTable,
};
pub trait Driver: Sync + Send + Debug + KObject {
    /// @brief: 获取驱动匹配表
    /// @parameter: None
    /// @return: 驱动匹配表
    /// 对于不需要匹配，在系统初始化的时候就生成的设备，例如 PlatformBus 就不需要匹配表
    fn compatible_table(&self) -> CompatibleTable {
        //TODO 要完善每个 CompatibleTable ，将来要把这个默认实现删除
        return CompatibleTable::new(vec!["unknown"]);
    }

    /// @brief 添加可支持的设备
    /// @parameter: device 新增的匹配项
    fn append_compatible_table(&self, _device: &CompatibleTable) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedOperation)
    }

    /// @brief 探测设备
    /// @param data 设备初始拥有的基本信息
    fn probe(&self, data: &DevicePrivateData) -> Result<(), DriverError>;

    /// @brief 加载设备，包括检查资源可用性，和注册到相应的管理器中。
    /// @param data 设备初始拥有的信息
    /// @param resource 设备可能申请的资源(或者像伪设备不需要就为None)
    fn load(
        &self,
        data: DevicePrivateData,
        resource: Option<DeviceResource>,
    ) -> Result<Arc<dyn Device>, DriverError>;

    /// @brief: 获取驱动标识符
    /// @parameter: None
    /// @return: 该驱动驱动唯一标识符
    fn id_table(&self) -> IdTable;
}
