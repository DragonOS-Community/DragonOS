use alloc::boxed::Box;
use system_error::SystemError;

use crate::{
    arch::kvm_arch_ops,
    kwarn,
    virt::vm::kvm_host::{io::KvmIoDeviceOps, vcpu::VirtCpu, Vm},
};

use apicdef::*;

mod apicdef;

#[derive(Debug)]
pub struct KvmLapic {
    pub base_address: usize,
    pub dev: Box<dyn KvmIoDeviceOps>,
    pub apicv_active: bool,
    pub regs: Box<[u8]>,
}

impl KvmLapic {
    const LAPIC_MMIO_LENGTH: usize = 1 << 12;

    pub fn apic_mmio_in_range(&self, addr: usize) -> bool {
        return addr >= self.base_address && addr < self.base_address + Self::LAPIC_MMIO_LENGTH;
    }

    pub fn kvm_lapic_reg_write(&self, reg: u32, val: u32) -> bool {
        let mut ret;
        match reg {
            _ => {
                kwarn!("kvm_lapic_reg_write: reg: {reg} not found");
                ret = false;
            }
        }
        return ret;
    }
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

#[derive(Debug)]
pub struct KvmApicMMioDev {}

impl KvmIoDeviceOps for KvmApicMMioDev {
    fn read(
        &self,
        vcpu: &VirtCpu,
        addr: usize,
        len: u32,
        val: &mut usize,
    ) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn write(
        &self,
        vcpu: &VirtCpu,
        addr: usize,
        len: u32,
        data: &usize,
    ) -> Result<(), SystemError> {
        let apic = vcpu.arch.apic.as_ref().unwrap();

        if !apic.apic_mmio_in_range(addr) {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let offset = addr - apic.base_address;

        if len != 4 || (offset & 0xf != 0) {
            return Ok(());
        }

        let val = unsafe { *((*data) as *const u32) };

        apic.kvm_lapic_reg_write((offset & 0xff0) as u32, val);
        return Ok(());
    }
}
