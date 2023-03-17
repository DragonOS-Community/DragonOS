use super::LocalAPIC;


#[derive(Debug)]
pub struct X2Apic;

impl LocalAPIC for X2Apic{
    fn support() -> bool {
        return x86::cpuid::CpuId::new().get_feature_info().expect("Get cpu feature info failed.").has_x2apic();
    }

    fn init_current_cpu(&self) -> bool {
        todo!()
    }

    fn send_eoi(&self) {
        todo!()
    }

    fn version(&self) -> u32 {
        todo!()
    }

    fn id(&self) -> u32 {
        todo!()
    }

    fn set_lvt(&self, register: super::LVTRegister, lvt:super::LVT) {
        todo!()
    }
}