use super::table::TSSManager;

#[no_mangle]
unsafe extern "C" fn set_current_core_tss(stack_start: usize, ist0: usize) {
    let current_tss = TSSManager::current_tss();
    current_tss.set_rsp(x86::Ring::Ring0, stack_start as u64);
    current_tss.set_ist(0, ist0 as u64);
}

#[no_mangle]
unsafe fn load_current_core_tss() {
    TSSManager::load_tr();
}
