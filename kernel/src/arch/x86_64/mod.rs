#[macro_use]
pub mod asm;
mod acpi;
mod c_adapter;
pub mod cpu;
pub mod driver;
pub mod fpu;
pub mod interrupt;
pub mod ipc;
pub mod kvm;
pub mod libs;
pub mod mm;
pub mod msi;
pub mod pci;
pub mod process;
pub mod rand;
pub mod sched;
pub mod setup;
pub mod smp;
pub mod syscall;
pub mod time;

pub use self::pci::pci::X86_64PciArch as PciArch;

/// 导出内存管理的Arch结构体
pub use self::mm::X86_64MMArch as MMArch;

pub use interrupt::X86_64InterruptArch as CurrentIrqArch;

pub use crate::arch::asm::pio::X86_64PortIOArch as CurrentPortIOArch;
pub use kvm::X86_64KVMArch as KVMArch;

pub use crate::arch::ipc::signal::X86_64SignalArch as CurrentSignalArch;
pub use crate::arch::time::X86_64TimeArch as CurrentTimeArch;
