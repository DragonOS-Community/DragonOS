use super::init_intertrait;

#[no_mangle]
unsafe extern "C" fn rs_init_intertrait() {
    init_intertrait();
}
