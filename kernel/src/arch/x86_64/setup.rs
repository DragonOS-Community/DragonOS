use system_error::SystemError;

use super::{acpi::early_acpi_boot_init, smp::X86_64_SMP_MANAGER};

/// 进行架构相关的初始化工作
pub fn setup_arch() -> Result<(), SystemError> {
    early_acpi_boot_init()?;
    X86_64_SMP_MANAGER.build_cpu_map()?;
    return Ok(());
}
