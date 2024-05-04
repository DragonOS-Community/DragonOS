use crate::{arch::kvm_arch_ops, virt::vm::kvm_host::vcpu::VirtCpu};

const APIC_DEFAULT_PHYS_BASE: u64 = 0xfee00000;
const MSR_IA32_APICBASE: u64 = 0x0000001b;
const MSR_IA32_APICBASE_BSP: u64 = (1 << 8);
const MSR_IA32_APICBASE_ENABLE: u64 = (1 << 11);
const MSR_IA32_APICBASE_BASE: u64 = (0xfffff << 12);

impl VirtCpu {
    pub fn lapic_reset(&mut self, init_event: bool) {
        let apic = self.arch.apic;

        kvm_arch_ops().apicv_pre_state_restore(self);

        if !init_event {
            let mut msr_val = APIC_DEFAULT_PHYS_BASE | MSR_IA32_APICBASE_ENABLE;
            if self.kvm().lock().arch.bsp_vcpu_id == self.vcpu_id {
                msr_val |= MSR_IA32_APICBASE_BSP;
            }
        }
    }

    fn lapic_set_base(&mut self, value: u64) {
        let old_val = self.arch.apic_base;
        let apic = self.arch.apic;

        self.arch.apic_base = value;

        if (old_val ^ value) & MSR_IA32_APICBASE_ENABLE != 0 {
            // TODO: kvm_update_cpuid_runtime(vcpu);
        }

        if apic.is_none() {
            return;
        }

        if (old_val ^ value) & MSR_IA32_APICBASE_ENABLE != 0 {
            if value & MSR_IA32_APICBASE_ENABLE != 0 {}
        }

        todo!()
    }
}
