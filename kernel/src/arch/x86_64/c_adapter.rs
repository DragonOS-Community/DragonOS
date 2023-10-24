use super::setup::setup_arch;

#[no_mangle]
unsafe extern "C" fn rs_setup_arch() -> i32 {
    return setup_arch()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());
}
