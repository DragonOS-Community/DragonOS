#[cfg(target_arch = "riscv64")]
pub mod timer_riscv;

pub mod acpi_pm;
#[cfg(target_arch = "x86_64")]
pub mod kvm_clock;
#[cfg(target_arch = "x86_64")]
pub mod tsc;
