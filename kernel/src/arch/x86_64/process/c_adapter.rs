use crate::kdebug;

use super::table::TSSManager;

#[no_mangle]
unsafe extern "C" fn set_current_core_tss(stack_start: usize, ist0: usize) {
    let current_tss = TSSManager::current_tss();
    kdebug!(
        "set_current_core_tss: stack_start={:#x}, ist0={:#x}\n",
        stack_start,
        ist0
    );
    current_tss.set_rsp(x86::Ring::Ring0, stack_start as u64);
    current_tss.set_ist(0, ist0 as u64);
}

#[no_mangle]
unsafe extern "C" fn rs_load_current_core_tss() {
    TSSManager::load_tr();
}
