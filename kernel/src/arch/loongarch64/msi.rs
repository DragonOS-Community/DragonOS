use crate::driver::pci::pci_irq::TriggerMode;

/// 获得MSI Message Address
///
/// # 参数
/// - `processor`: 目标CPU ID号
///
/// # 返回值
/// MSI Message Address
pub fn arch_msi_message_address(_processor: u16) -> u32 {
    unimplemented!("loongarch64::arch_msi_message_address()")
}
/// 获得MSI Message Data
///
/// # 参数
/// - `vector`: 分配的中断向量号
/// - `processor`: 目标CPU ID号
/// - `trigger`: 申请中断的触发模式，MSI默认为边沿触发
///
/// # 返回值
/// MSI Message Address
pub fn arch_msi_message_data(_vector: u16, _processor: u16, _trigger: TriggerMode) -> u32 {
    unimplemented!("loongarch64::arch_msi_message_data()")
}
