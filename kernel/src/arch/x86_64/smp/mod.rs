use core::{arch::asm, hint::spin_loop, sync::atomic::compiler_fence};

use memoffset::offset_of;

use crate::{
    arch::process::table::TSSManager, exception::InterruptArch, kdebug, process::ProcessManager,
    smp::core::smp_get_processor_id, include::bindings::bindings::cpu_core_info,
};

use super::CurrentIrqArch;

extern "C" {
    fn smp_ap_start_stage2();
}

#[repr(C)]
struct ApStartStackInfo {
    vaddr: usize,
}

/// AP处理器启动时执行
#[no_mangle]
unsafe extern "C" fn smp_ap_start() -> ! {
    CurrentIrqArch::interrupt_disable();
    let vaddr = cpu_core_info[smp_get_processor_id() as usize].stack_start as usize;
    compiler_fence(core::sync::atomic::Ordering::SeqCst);
    let v = ApStartStackInfo { vaddr };
    smp_init_switch_stack(&v);
}

#[naked]
unsafe extern "sysv64" fn smp_init_switch_stack(st: &ApStartStackInfo) -> ! {
    asm!(concat!("
        mov rsp, [rdi + {off_rsp}]
        mov rbp, [rdi + {off_rsp}]
        jmp {stage1}
    "), 
        off_rsp = const(offset_of!(ApStartStackInfo, vaddr)),
        stage1 = sym smp_ap_start_stage1, 
    options(noreturn));
}

unsafe extern "C" fn smp_ap_start_stage1() -> ! {
    let id = smp_get_processor_id();
    kdebug!("smp_ap_start_stage1: id: {}\n", id);
    let current_idle = ProcessManager::idle_pcb()[smp_get_processor_id() as usize].clone();

    let tss = TSSManager::current_tss();

    tss.set_rsp(
        x86::Ring::Ring0,
        current_idle.kernel_stack().stack_max_address().data() as u64,
    );
    TSSManager::load_tr();

    smp_ap_start_stage2();
    loop {
        spin_loop();
    }
}
