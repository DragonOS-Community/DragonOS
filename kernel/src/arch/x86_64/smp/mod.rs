use core::{arch::asm, hint::spin_loop, sync::atomic::compiler_fence};

use memoffset::offset_of;

use crate::{
    arch::process::table::TSSManager, exception::InterruptArch, kdebug, process::ProcessManager,
    smp::core::smp_get_processor_id,
};

use super::CurrentIrqArch;

extern "C" {
    fn smp_ap_start_stage2();
}

/// AP处理器启动时执行
#[no_mangle]
unsafe extern "C" fn smp_ap_start() -> ! {
    CurrentIrqArch::interrupt_disable();
    let id = smp_get_processor_id();
    kdebug!("smp_ap_start: id: {}\n", id);
    
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
