use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, Ordering},
};

use raw_cpuid::CpuId;
use x86::msr::{rdmsr, wrmsr, IA32_APIC_BASE, IA32_TSC_AUX, IA32_X2APIC_APICID};

use crate::{arch::smp::SMP_BOOT_DATA, smp::cpu::ProcessorId};

static TSC_AUX_CPU_ID_READY: AtomicBool = AtomicBool::new(false);
static TSC_AUX_CPU_ID_UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// Store the logical CPU number in the architectural per-CPU TSC_AUX MSR.
///
/// This is called once by each CPU while the APIC-based lookup is still in
/// use. Fast reads are enabled globally only after every online CPU has
/// completed this initialization.
pub(crate) fn init_tsc_aux_cpu_id(cpu: ProcessorId) {
    if CpuId::new()
        .get_extended_processor_and_feature_identifiers()
        .is_some_and(|features| features.has_rdtscp())
    {
        unsafe { wrmsr(IA32_TSC_AUX, cpu.data() as u64) };
    } else {
        TSC_AUX_CPU_ID_UNSUPPORTED.store(true, Ordering::Release);
        TSC_AUX_CPU_ID_READY.store(false, Ordering::Release);
    }
}

pub(crate) fn enable_tsc_aux_cpu_id() {
    if !TSC_AUX_CPU_ID_UNSUPPORTED.load(Ordering::Acquire) {
        TSC_AUX_CPU_ID_READY.store(true, Ordering::Release);
    }
}

/// 获取当前CPU的逻辑编号
#[inline]
pub fn current_cpu_id() -> ProcessorId {
    if !SMP_BOOT_DATA.is_initialized() {
        return ProcessorId::new(0);
    }

    if TSC_AUX_CPU_ID_READY.load(Ordering::Acquire) {
        let (_, cpu) = unsafe { x86::time::rdtscp() };
        return ProcessorId::new(cpu);
    }

    current_cpu_id_slow()
}

/// APIC/CPUID based lookup used while bringing up a CPU before its TSC_AUX
/// slot has been initialized. This must not consult the global fast-path flag.
pub(crate) fn current_cpu_id_slow() -> ProcessorId {
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
