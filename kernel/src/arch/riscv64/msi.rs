use crate::driver::pci::pci_irq::TriggerMode;

/// @brief 获得MSI Message Address
/// @param processor 目标CPU ID号
/// @return MSI Message Address
pub fn arch_msi_message_address(_processor: u16) -> u32 {
    unimplemented!("riscv64::arch_msi_message_address()")
}
/// @brief 获得MSI Message Data
/// @param vector 分配的中断向量号
/// @param processor 目标CPU ID号
/// @param trigger  申请中断的触发模式，MSI默认为边沿触发
/// @return MSI Message Address
pub fn arch_msi_message_data(_vector: u16, _processor: u16, _trigger: TriggerMode) -> u32 {
    unimplemented!("riscv64::arch_msi_message_data()")
}
