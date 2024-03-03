use system_error::SystemError;

use crate::{driver::acpi::acpi_manager, kinfo, mm::percpu::PerCpu, smp::cpu::ProcessorId};

use super::smp::SMP_BOOT_DATA;

pub(super) fn early_acpi_boot_init() -> Result<(), SystemError> {
    // 在这里解析madt，初始化smp boot data

    let platform_info = acpi_manager().platform_info().ok_or(SystemError::ENODEV)?;
    let processor_info = platform_info.processor_info.ok_or(SystemError::ENODEV)?;

    unsafe {
        SMP_BOOT_DATA.set_phys_id(
            ProcessorId::new(0),
            processor_info.boot_processor.local_apic_id as usize,
        );
        let mut cnt = ProcessorId::new(1);
        for ap in processor_info.application_processors.iter() {
            if cnt.data() >= PerCpu::MAX_CPU_NUM {
                break;
            }
            SMP_BOOT_DATA.set_phys_id(cnt, ap.local_apic_id as usize);
            cnt = ProcessorId::new(cnt.data() + 1);
        }
        SMP_BOOT_DATA.set_cpu_count(cnt.data());
        SMP_BOOT_DATA.mark_initialized();
    }
    kinfo!(
        "early_acpi_boot_init: cpu_count: {}\n",
        SMP_BOOT_DATA.cpu_count()
    );

    // todo!("early_acpi_boot_init")
    return Ok(());
}
