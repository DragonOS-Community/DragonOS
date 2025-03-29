pub mod asm;
pub mod cpu;
pub mod driver;
pub mod elf;
pub mod init;
pub mod interrupt;
pub mod ipc;
pub mod kprobe;
mod kvm;
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

pub use self::interrupt::RiscV64InterruptArch as CurrentIrqArch;
pub use self::kvm::RiscV64KVMArch as KVMArch;
pub use self::mm::RiscV64MMArch as MMArch;
pub use self::pci::RiscV64PciArch as PciArch;
pub use self::pio::RiscV64PortIOArch as CurrentPortIOArch;
pub use self::time::RiscV64TimeArch as CurrentTimeArch;

pub use self::elf::RiscV64ElfArch as CurrentElfArch;

pub use self::ipc::signal::RiscV64SignalArch as CurrentSignalArch;

pub use crate::arch::smp::RiscV64SMPArch as CurrentSMPArch;

pub use crate::arch::sched::RiscV64SchedArch as CurrentSchedArch;

pub fn panic_pre_work() {
    unsafe { riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Initial) };
}
pub fn panic_post_work() {
    unsafe { riscv::register::sstatus::set_fs(riscv::register::sstatus::FS::Off) };
}
