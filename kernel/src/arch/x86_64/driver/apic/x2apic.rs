use core::sync::atomic::{fence, Ordering};

use log::info;
use x86::msr::{
    rdmsr, wrmsr, IA32_APIC_BASE, IA32_X2APIC_APICID, IA32_X2APIC_EOI, IA32_X2APIC_SIVR,
    IA32_X2APIC_VERSION,
};

use super::{hw_irq::ApicId, LVTRegister, LocalAPIC, LVT};

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
            wrmsr(IA32_APIC_BASE, rdmsr(IA32_APIC_BASE) | 1 << 10);

            assert!(
                (rdmsr(IA32_APIC_BASE) & 0xc00) == 0xc00,
                "x2APIC enable failed."
            );

            // 设置Spurious-Interrupt Vector Register
            {
                let val = if self.support_eoi_broadcast_suppression() {
                    (1 << 12) | (1 << 8)
                } else {
                    1 << 8
                };

                wrmsr(IA32_X2APIC_SIVR, val);

                assert!(
                    (rdmsr(IA32_X2APIC_SIVR) & 0x100) == 0x100,
                    "x2APIC software enable failed."
                );
                info!("x2APIC software enabled.");

                if self.support_eoi_broadcast_suppression() {
                    assert!(
                        (rdmsr(IA32_X2APIC_SIVR) & 0x1000) == 0x1000,
                        "x2APIC EOI broadcast suppression enable failed."
                    );
                    info!("x2APIC EOI broadcast suppression enabled.");
                }
            }
            // debug!("x2apic: to mask all lvt");
            self.mask_all_lvt();
            // debug!("x2apic: all lvt masked");
        }
        true
    }

    /// 发送 EOI (End Of Interrupt)
    fn send_eoi(&self) {
        fence(Ordering::SeqCst);
        unsafe {
            wrmsr(IA32_X2APIC_EOI, 0);
        }

        fence(Ordering::SeqCst);
    }

    /// 获取 x2APIC 版本
    fn version(&self) -> u8 {
        unsafe { (rdmsr(IA32_X2APIC_VERSION) & 0xff) as u8 }
    }

    fn support_eoi_broadcast_suppression(&self) -> bool {
        unsafe { ((rdmsr(IA32_X2APIC_VERSION) >> 24) & 1) == 1 }
    }

    fn max_lvt_entry(&self) -> u8 {
        unsafe { ((rdmsr(IA32_X2APIC_VERSION) >> 16) & 0xff) as u8 + 1 }
    }

    /// 获取 x2APIC 的 APIC ID
    fn id(&self) -> ApicId {
        unsafe { ApicId::new(rdmsr(IA32_X2APIC_APICID) as u32) }
    }

    /// 设置 Local Vector Table (LVT) 寄存器
    fn set_lvt(&mut self, lvt: LVT) {
        unsafe {
            wrmsr(lvt.register().into(), lvt.data as u64);
        }
    }

    fn read_lvt(&self, reg: LVTRegister) -> LVT {
        unsafe { LVT::new(reg, (rdmsr(reg.into()) & 0xffff_ffff) as u32).unwrap() }
    }

    fn mask_all_lvt(&mut self) {
        // self.set_lvt(LVT::new(LVTRegister::CMCI, LVT::MASKED).unwrap());
        let cpuid = raw_cpuid::CpuId::new();
        // cpuid.get_performance_monitoring_info();
        self.set_lvt(LVT::new(LVTRegister::Timer, LVT::MASKED).unwrap());

        if cpuid.get_thermal_power_info().is_some() {
            self.set_lvt(LVT::new(LVTRegister::Thermal, LVT::MASKED).unwrap());
        }

        if cpuid.get_performance_monitoring_info().is_some() {
            self.set_lvt(LVT::new(LVTRegister::PerformanceMonitor, LVT::MASKED).unwrap());
        }

        self.set_lvt(LVT::new(LVTRegister::LINT0, LVT::MASKED).unwrap());
        self.set_lvt(LVT::new(LVTRegister::LINT1, LVT::MASKED).unwrap());

        self.set_lvt(LVT::new(LVTRegister::ErrorReg, LVT::MASKED).unwrap());
    }

    fn write_icr(&self, icr: x86::apic::Icr) {
        unsafe { wrmsr(0x830, ((icr.upper() as u64) << 32) | icr.lower() as u64) };
    }
}
