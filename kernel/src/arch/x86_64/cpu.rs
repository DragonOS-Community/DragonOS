use core::hint::spin_loop;

use raw_cpuid::CpuId;
use x86::msr::{rdmsr, IA32_APIC_BASE, IA32_X2APIC_APICID};

use crate::{arch::smp::SMP_BOOT_DATA, smp::cpu::ProcessorId};

/// 获取当前CPU的逻辑编号
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    if !SMP_BOOT_DATA.is_initialized() {
        return ProcessorId::new(0);
    }

    let x2apic_id = current_x2apic_physical_id();
    if let Some(apic_id) = x2apic_id {
        if let Some(cpu_id) = phys_apic_id_to_cpu_id(apic_id) {
            return cpu_id;
        }
    }

    let cpuid_apic_id = current_apic_physical_id();
    if let Some(cpu_id) = phys_apic_id_to_cpu_id(cpuid_apic_id) {
        return cpu_id;
    }

    panic!(
        "current cpu apic id is not present in SMP_BOOT_DATA: x2apic={x2apic_id:?}, cpuid={cpuid_apic_id}, bsp_phys_id={}",
        SMP_BOOT_DATA.bsp_phys_id()
    );
}

/// 获取当前CPU的物理APIC ID
#[inline]
pub fn current_apic_physical_id() -> usize {
    let cpuid = CpuId::new();

    if let Some(topology) = cpuid.get_extended_topology_info() {
        if let Some(apic_id) = topology.map(|level| level.x2apic_id()).next() {
            return apic_id as usize;
        }
    }

    if let Some(topology) = cpuid.get_processor_topology_info() {
        return topology.x2apic_id() as usize;
    }

    if let Some(feature_info) = cpuid.get_feature_info() {
        return feature_info.initial_local_apic_id() as usize;
    }

    return 0;
}

#[inline]
fn current_x2apic_physical_id() -> Option<usize> {
    let apic_base = unsafe { rdmsr(IA32_APIC_BASE) };
    if (apic_base & (1 << 10)) == 0 {
        return None;
    }

    Some(unsafe { rdmsr(IA32_X2APIC_APICID) as usize })
}

#[inline]
pub fn phys_apic_id_to_cpu_id(apic_id: usize) -> Option<ProcessorId> {
    if !SMP_BOOT_DATA.is_initialized() {
        return Some(ProcessorId::new(apic_id as u32));
    }

    for cpu in 0..SMP_BOOT_DATA.cpu_count() {
        if SMP_BOOT_DATA.phys_id(cpu) == apic_id {
            return Some(ProcessorId::new(cpu as u32));
        }
    }

    None
}

/// 重置cpu
#[allow(dead_code)]
pub unsafe fn cpu_reset() -> ! {
    // 重启计算机
    unsafe { x86::io::outb(0x64, 0xfe) };
    loop {
        spin_loop();
    }
}
