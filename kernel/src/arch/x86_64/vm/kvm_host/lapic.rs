use alloc::boxed::Box;

use crate::{
    arch::kvm_arch_ops,
    virt::vm::kvm_host::{vcpu::VirtCpu, Vm},
};

const APIC_DEFAULT_PHYS_BASE: u64 = 0xfee00000;
#[allow(dead_code)]
const MSR_IA32_APICBASE: u64 = 0x0000001b;
const MSR_IA32_APICBASE_BSP: u64 = 1 << 8;
const MSR_IA32_APICBASE_ENABLE: u64 = 1 << 11;
#[allow(dead_code)]
const MSR_IA32_APICBASE_BASE: u64 = 0xfffff << 12;

#[derive(Debug)]
pub struct KvmLapic {
    pub apicv_active: bool,
    pub regs: Box<[u8]>,
}

impl VirtCpu {
    pub fn lapic_reset(&mut self, vm: &Vm, init_event: bool) {
        kvm_arch_ops().apicv_pre_state_restore(self);

        if !init_event {
            let mut msr_val = APIC_DEFAULT_PHYS_BASE | MSR_IA32_APICBASE_ENABLE;
            if vm.arch.bsp_vcpu_id == self.vcpu_id {
                msr_val |= MSR_IA32_APICBASE_BSP;
            }

            self.lapic_set_base(msr_val);
        }

        if self.arch.apic.is_none() {
            return;
        }

        todo!()
    }

    fn lapic_set_base(&mut self, value: u64) {
        let old_val = self.arch.apic_base;
        let apic = self.arch.apic.as_ref();

        self.arch.apic_base = value;

        if (old_val ^ value) & MSR_IA32_APICBASE_ENABLE != 0 {
            // TODO: kvm_update_cpuid_runtime(vcpu);
        }

        if apic.is_none() {
            return;
        }

        if (old_val ^ value) & MSR_IA32_APICBASE_ENABLE != 0 {
            // if value & MSR_IA32_APICBASE_ENABLE != 0 {}
        }

        todo!()
    }
}
