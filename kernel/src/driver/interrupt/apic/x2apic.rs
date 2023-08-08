use x86::msr::{
    rdmsr, wrmsr, IA32_APIC_BASE, IA32_X2APIC_APICID, IA32_X2APIC_EOI, IA32_X2APIC_SIVR,
    IA32_X2APIC_VERSION,
};
// 调用了msr库，较完备

use super::{LVTRegister, LocalAPIC, LVT};

#[derive(Debug)]
pub struct X2Apic;

impl LocalAPIC for X2Apic {
    /// @brief 判断处理器是否支持x2APIC
    fn support() -> bool {
        return x86::cpuid::CpuId::new()
            .get_feature_info()
            .expect("Get cpu feature info failed.")
            .has_x2apic();
    }
/// @return true -> the function works
fn init_current_cpu(&mut self) -> bool {
    unsafe {
        // 设置 x2APIC 使能位
        wrmsr(
            IA32_APIC_BASE.into(),
            rdmsr(IA32_APIC_BASE.into()) | 1 << 10,
        );
        // 设置中断向量寄存器
        wrmsr(IA32_X2APIC_SIVR.into(), 0x100);
    }
    true
}

/// 发送 EOI (End Of Interrupt)
fn send_eoi(&mut self) {
    unsafe {
        wrmsr(IA32_X2APIC_EOI.into(), 0);
    }
}

/// 获取 x2APIC 版本
fn version(&self) -> u32 {
    unsafe { rdmsr(IA32_X2APIC_VERSION.into()) as u32 }
}

/// 获取 x2APIC 的 APIC ID
fn id(&self) -> u32 {
    unsafe { rdmsr(IA32_X2APIC_APICID.into()) as u32 }
}

/// 设置 Local Vector Table (LVT) 寄存器
fn set_lvt(&mut self, register: LVTRegister, lvt: LVT) {
    unsafe {
        wrmsr(register.into(), lvt.data as u64);
    }
}
}
