pub mod x86_64;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::pci::pci::X86_64PciArch as PciArch;
#[cfg(target_arch = "x86_64")]
pub use self::x86_64::*; //公开x86_64架构下的函数，使外界接口统一
use crate::driver::pci::pci::{BusDeviceFunction, PciError, PciRoot, SegmentGroupNumber};
/// TraitPciArch Pci架构相关函数，任何架构都应独立实现trait里的函数
pub trait TraitPciArch {
    /// @brief 读取寄存器值，x86_64架构通过读取两个特定io端口实现
    /// @param bus_device_function 设备的唯一标识符
    /// @param offset 寄存器偏移值
    /// @return 读取到的值
    fn read_config(bus_device_function: &BusDeviceFunction, offset: u8) -> u32;
    /// @brief 写入寄存器值，x86_64架构通过读取两个特定io端口实现
    /// @param bus_device_function 设备的唯一标识符
    /// @param offset 寄存器偏移值
    /// @param data 要写入的值
    fn write_config(bus_device_function: &BusDeviceFunction, offset: u8, data: u32);
    /// @brief PCI域地址到存储器域地址的转换,x86_64架构为一一对应
    /// @param address PCI域地址
    /// @return  Result<usize, PciError> 转换结果或出错原因
    fn address_pci_to_address_memory(address: usize) -> Result<usize, PciError>;
    /// @brief 获取Segement的root地址,x86_64架构为acpi mcfg表中读取
    /// @param segement 组id
    /// @return  Result<PciRoot, PciError> 转换结果或出错原因
    fn ecam_root(segement: SegmentGroupNumber) -> Result<PciRoot, PciError>;
}
