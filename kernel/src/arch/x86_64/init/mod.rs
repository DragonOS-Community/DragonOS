use core::{
    hint::spin_loop,
    sync::atomic::{compiler_fence, Ordering},
};

use log::debug;
use system_error::SystemError;
use x86::dtables::DescriptorTablePointer;

use crate::{
    arch::{interrupt::trap::arch_trap_init, process::table::TSSManager},
    driver::clocksource::acpi_pm::init_acpi_pm_clocksource,
    init::init::start_kernel,
    mm::{MemoryManagementArch, PhysAddr},
};

use self::boot::early_boot_init;

use super::{
    driver::{
        hpet::{hpet_init, hpet_instance},
        tsc::TSCManager,
    },
    MMArch,
};

mod boot;
mod multiboot2;
mod pvh;

#[derive(Debug)]
pub struct ArchBootParams {}

impl ArchBootParams {
    pub const DEFAULT: Self = ArchBootParams {};
}

extern "C" {
    static mut GDT_Table: [usize; 0usize];
    static mut IDT_Table: [usize; 0usize];
    fn head_stack_start();

}

#[no_mangle]
#[allow(static_mut_refs)]
unsafe extern "C" fn kernel_main(
    mb2_info: u64,
    mb2_magic: u64,
    bsp_gdt_size: u64,
    bsp_idt_size: u64,
    boot_entry_type: u64,
) -> ! {
    let mut gdtp = DescriptorTablePointer::<usize>::default();
    let gdt_vaddr =
        MMArch::phys_2_virt(PhysAddr::new(&GDT_Table as *const usize as usize)).unwrap();
    let idt_vaddr =
        MMArch::phys_2_virt(PhysAddr::new(&IDT_Table as *const usize as usize)).unwrap();
    gdtp.base = gdt_vaddr.data() as *const usize;
    gdtp.limit = bsp_gdt_size as u16 - 1;

    let idtp = DescriptorTablePointer::<usize> {
        base: idt_vaddr.data() as *const usize,
        limit: bsp_idt_size as u16 - 1,
    };

    x86::dtables::lgdt(&gdtp);
    x86::dtables::lidt(&idtp);

    compiler_fence(Ordering::SeqCst);
    if early_boot_init(boot_entry_type, mb2_magic, mb2_info).is_err() {
        loop {
            spin_loop();
        }
    }
    compiler_fence(Ordering::SeqCst);

    start_kernel();
}

/// 在内存管理初始化之前的架构相关的早期初始化
#[inline(never)]
#[allow(static_mut_refs)]
pub fn early_setup_arch() -> Result<(), SystemError> {
    let stack_start = unsafe { *(head_stack_start as *const u64) } as usize;
    debug!("head_stack_start={:#x}\n", stack_start);
    unsafe {
        let gdt_vaddr =
            MMArch::phys_2_virt(PhysAddr::new(&GDT_Table as *const usize as usize)).unwrap();
        let idt_vaddr =
            MMArch::phys_2_virt(PhysAddr::new(&IDT_Table as *const usize as usize)).unwrap();

        debug!("GDT_Table={:?}, IDT_Table={:?}\n", gdt_vaddr, idt_vaddr);
    }

    set_current_core_tss(stack_start, 0);
    unsafe { TSSManager::load_tr() };
    arch_trap_init().expect("arch_trap_init failed");

    return Ok(());
}

/// 架构相关的初始化
#[inline(never)]
pub fn setup_arch() -> Result<(), SystemError> {
    return Ok(());
}

/// 架构相关的初始化（在IDLE的最后一个阶段）
#[inline(never)]
pub fn setup_arch_post() -> Result<(), SystemError> {
    let ret = hpet_init();
    if ret.is_ok() {
        hpet_instance().hpet_enable().expect("hpet enable failed");
    } else {
        init_acpi_pm_clocksource().expect("acpi_pm_timer inits failed");
    }
    TSCManager::init().expect("tsc init failed");

    return Ok(());
}

fn set_current_core_tss(stack_start: usize, ist0: usize) {
    let current_tss = unsafe { TSSManager::current_tss() };
    debug!(
        "set_current_core_tss: stack_start={:#x}, ist0={:#x}\n",
        stack_start, ist0
    );
    current_tss.set_rsp(x86::Ring::Ring0, stack_start as u64);
    current_tss.set_ist(0, ist0 as u64);
}
