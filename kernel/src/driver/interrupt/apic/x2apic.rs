use x86::msr::{
    rdmsr, wrmsr, IA32_APIC_BASE, IA32_X2APIC_APICID, IA32_X2APIC_EOI, IA32_X2APIC_SIVR,
    IA32_X2APIC_VERSION,
};

use super::{LVTRegister, LocalAPIC, LVT};

#[derive(Debug)]
pub struct X2Apic;

impl LocalAPIC for X2Apic {
    fn support() -> bool {
        return x86::cpuid::CpuId::new()
            .get_feature_info()
            .expect("Get cpu feature info failed.")
            .has_x2apic();
    }

    fn init_current_cpu(&mut self) -> bool {
        unsafe {
            wrmsr(
                IA32_APIC_BASE.into(),
                rdmsr(IA32_APIC_BASE.into()) | 1 << 10,
            );
            wrmsr(IA32_X2APIC_SIVR.into(), 0x100);
        }
        true
    }

    fn send_eoi(&mut self) {
        unsafe {
            wrmsr(IA32_X2APIC_EOI.into(), 0);
        }
    }

    fn version(&self) -> u32 {
        unsafe { rdmsr(IA32_X2APIC_VERSION.into()) as u32 }
    }

    fn id(&self) -> u32 {
        unsafe { rdmsr(IA32_X2APIC_APICID.into()) as u32 }
    }

    fn set_lvt(&mut self, register: LVTRegister, lvt: LVT) {
        unsafe {
            wrmsr(register.into(), lvt.data as u64);
        }
    }
}
