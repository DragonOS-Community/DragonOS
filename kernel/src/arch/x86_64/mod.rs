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
pub mod syscall;

pub use interrupt::X86_64InterruptArch as CurrentIrqArch;
