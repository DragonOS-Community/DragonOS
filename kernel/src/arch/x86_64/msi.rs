use crate::driver::pci::pci_irq::TriggerMode;
/// @brief 获得MSI Message Address
/// @param processor 目标CPU ID号
/// @return MSI Message Address
pub fn arch_msi_message_address(processor: u16) -> u32 {
    0xfee00000 | ((processor as u32) << 12)
}
/// @brief 获得MSI Message Data
/// @param vector 分配的中断向量号
/// @param processor 目标CPU ID号
/// @param trigger  申请中断的触发模式，MSI默认为边沿触发
/// @return MSI Message Address
pub fn arch_msi_message_data(vector: u16, _processor: u16, trigger: TriggerMode) -> u32 {
    match trigger {
        TriggerMode::EdgeTrigger => vector as u32,
        TriggerMode::AssertHigh => vector as u32 | 1 << 15 | 1 << 14,
        TriggerMode::AssertLow => vector as u32 | 1 << 15,
    }
}
