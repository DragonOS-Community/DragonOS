#[macro_use]
pub mod asm;
pub mod context;
pub mod cpu;
pub mod fpu;
pub mod interrupt;
pub mod mm;
pub mod pci;
pub mod rand;
pub mod sched;

pub use self::pci::pci::X86_64PciArch as PciArch;

pub use self::mm::X86_64MMArch as MMArch;
pub use interrupt::X86_64InterruptArch as CurrentIrqArch;
