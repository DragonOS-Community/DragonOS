use super::{LocalAPIC, LVT, LVTRegister};

#[derive(Debug)]
pub struct XApic {
    /// xAPIC的MMIO空间起始地址
    map_vaddr: usize,
}

impl LocalAPIC for XApic {
    /// @brief 判断处理器是否支持apic
    fn support() -> bool {
        return x86::cpuid::CpuId::new()
            .get_feature_info()
            .expect("Get cpu feature info failed.")
            .has_apic();
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

    fn set_lvt(&self, register: LVTRegister, lvt:LVT) {
        todo!()
    }
}

/// @brief local APIC 寄存器地址偏移量
#[derive(Debug)]
#[allow(dead_code)]
enum LocalApicOffset {
    ID = 0x20,
    Version = 0x30,
    TPR = 0x80,
    APR = 0x90,
    PPR = 0xa0,
    EOI = 0xb0,
    RRD = 0xc0,
    LDR = 0xd0,
    DFR = 0xe0,
    SVR = 0xf0,
}
