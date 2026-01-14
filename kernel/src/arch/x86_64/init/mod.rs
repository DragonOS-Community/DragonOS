use core::{
    hint::spin_loop,
    sync::atomic::{compiler_fence, Ordering},
};

use log::warn;

use log::debug;
use system_error::SystemError;
use x86::dtables::DescriptorTablePointer;

use crate::{
    arch::{fpu::FpState, interrupt::trap::arch_trap_init, process::table::TSSManager},
    driver::clocksource::{acpi_pm::init_acpi_pm_clocksource, kvm_clock::init_kvm_clocksource},
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

mod boot_params;

pub use self::boot_params::ArchBootParams;

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
    // 初始化 XSAVE 支持（必须在任何 FPU 状态保存/恢复之前）
    FpState::init_xsave_support();

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
    // First, try to initialize KVM clock if running on KVM
    // KVM clock has priority as it's the most accurate for virtualized environments
    if init_kvm_clocksource().is_ok() {
        debug!("KVM clock initialized successfully");
    } else {
        debug!("KVM clock not available, falling back to hardware timers");

        // Try to initialize HPET
        match hpet_init() {
            Ok(_) => {
                debug!("HPET initialized successfully");
                if let Err(e) = hpet_instance().hpet_enable() {
                    warn!("HPET enable failed: {:?}, trying ACPI PM Timer", e);
                    // Try ACPI PM Timer as fallback
                    if let Err(e) = init_acpi_pm_clocksource() {
                        warn!("ACPI PM Timer init failed: {:?}, will rely on TSC/jiffies", e);
                    } else {
                        debug!("ACPI PM Timer initialized successfully");
                    }
                }
            }
            Err(e) => {
                debug!("HPET init failed: {:?}, trying ACPI PM Timer", e);
                // Try ACPI PM Timer as fallback
                if let Err(e) = init_acpi_pm_clocksource() {
                    warn!("ACPI PM Timer init failed: {:?}, will rely on TSC/jiffies", e);
                } else {
                    debug!("ACPI PM Timer initialized successfully");
                }
            }
        }
    }

    // TSC initialization is critical for x86_64
    if let Err(e) = TSCManager::init() {
        warn!("TSC init failed: {:?}, system may have timing issues", e);
    }

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
