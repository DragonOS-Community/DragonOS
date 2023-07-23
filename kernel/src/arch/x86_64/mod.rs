#[macro_use]
pub mod asm;
pub mod context;
pub mod cpu;
pub mod fpu;
pub mod interrupt;
pub mod libs;
pub mod mm;
pub mod msi;
pub mod pci;
pub mod rand;
pub mod sched;
pub mod syscall;
pub mod kvm;

pub use self::pci::pci::X86_64PciArch as PciArch;

/// 导出内存管理的Arch结构体
pub use self::mm::X86_64MMArch as MMArch;

pub use interrupt::X86_64InterruptArch as CurrentIrqArch;
