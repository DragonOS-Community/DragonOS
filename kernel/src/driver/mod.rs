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

use crate::filesystem::vfs::IndexNode;

use self::base::{
    device::{driver::DriverError, Device, DevicePrivateData, DeviceResource, IdTable},
    platform::CompatibleTable,
};
pub trait Driver: Sync + Send + Debug {
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any;

    //对于不需要匹配，在系统初始化的时候就生成的设备，例如 PlatformBus 就不需要匹配表

    /// @brief: 获取驱动匹配表
    /// @parameter: None
    /// @return: 驱动匹配表
    fn compatible_table(&self) -> CompatibleTable {
        //TODO 要完善每个 CompatibleTable ，将来要把这个默认实现删除
        return CompatibleTable::new(vec!["unknown"]);
    }

    /// @brief 添加可支持的设备
    /// @parameter: device 新增的匹配项
    fn append_compatible_table(&self, device: &CompatibleTable) -> Result<(), DriverError> {
        Err(DriverError::UnsupportedOperation)
    }

    /// @brief 探测设备
    /// @param data 设备初始拥有的基本信息
    fn probe(&self, data: DevicePrivateData) -> Result<(), DriverError>;

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

    // 考虑到很多驱动并不需要存储在系统中，只需要当工具人就可以了，因此 SysINode 是可选的
    /// @brief: 设置驱动的sys information
    /// @parameter id_table: 驱动标识符，用于唯一标识该驱动
    /// @return: 驱动实例
    fn set_sys_info(&self, sys_info: Option<Arc<dyn IndexNode>>) {}

    /// @brief: 获取驱动的sys information
    /// @parameter id_table: 驱动标识符，用于唯一标识该驱动
    /// @return: 驱动实例
    fn sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        return None;
    }
}
