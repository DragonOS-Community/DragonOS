use super::{init_before_mem_init, init_intertrait};

#[no_mangle]
unsafe extern "C" fn rs_init_intertrait() {
    init_intertrait();
}

#[no_mangle]
unsafe extern "C" fn rs_init_before_mem_init() {
    init_before_mem_init();
}
