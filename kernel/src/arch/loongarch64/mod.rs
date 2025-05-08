pub mod asm;
pub mod cpu;
pub mod elf;
pub mod filesystem;
pub mod init;
pub mod interrupt;
pub mod ipc;
pub mod kprobe;
pub mod mm;
pub mod msi;
pub mod pci;
pub mod pio;
pub mod process;
pub mod rand;
pub mod sched;
pub mod smp;
pub mod syscall;
pub mod time;

pub use self::elf::LoongArch64ElfArch as CurrentElfArch;
pub use self::interrupt::LoongArch64InterruptArch as CurrentIrqArch;
pub use self::ipc::signal::LoongArch64SignalArch as CurrentSignalArch;
pub use self::mm::LoongArch64MMArch as MMArch;
pub use self::pci::LoongArch64PciArch as PciArch;
pub use self::pio::LoongArch64PortIOArch as CurrentPortIOArch;
pub use self::sched::LoongArch64SchedArch as CurrentSchedArch;
pub use self::smp::LoongArch64SMPArch as CurrentSMPArch;
pub use self::time::LoongArch64TimeArch as CurrentTimeArch;

pub fn panic_pre_work() {}
pub fn panic_post_work() {}
