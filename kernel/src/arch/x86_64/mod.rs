#[macro_use]
pub mod asm;
mod acpi;
pub mod cpu;
pub mod driver;
pub mod elf;
pub mod fpu;
pub mod init;
pub mod interrupt;
pub mod ipc;
pub mod kprobe;
pub mod libs;
pub mod mm;
pub mod msi;
pub mod pci;
pub mod process;
pub mod rand;
pub mod sched;
pub mod smp;
pub mod syscall;
pub mod time;
pub mod vm;

pub use self::pci::pci::X86_64PciArch as PciArch;

/// 导出内存管理的Arch结构体
pub use self::mm::X86_64MMArch as MMArch;

pub use interrupt::X86_64InterruptArch as CurrentIrqArch;

pub use crate::arch::asm::pio::X86_64PortIOArch as CurrentPortIOArch;

#[allow(unused_imports)]
pub use crate::arch::ipc::signal::X86_64SignalArch as CurrentSignalArch;
pub use crate::arch::time::X86_64TimeArch as CurrentTimeArch;

pub use crate::arch::elf::X86_64ElfArch as CurrentElfArch;

pub use crate::arch::smp::X86_64SMPArch as CurrentSMPArch;

pub use crate::arch::sched::X86_64SchedArch as CurrentSchedArch;

pub use crate::arch::vm::KvmArchManager as CurrentKvmManager;

pub use crate::arch::vm::kvm_host::X86KvmArch as KvmArch;

pub use crate::arch::vm::x86_kvm_ops as kvm_arch_ops;

pub use crate::arch::vm::kvm_host::vcpu::X86VcpuArch as VirtCpuArch;
pub use crate::arch::vm::kvm_host::KvmVcpuStat as VirtCpuStat;
