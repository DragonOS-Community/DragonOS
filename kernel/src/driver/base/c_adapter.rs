use super::init::driver_init;

#[no_mangle]
unsafe extern "C" fn rs_driver_init() -> i32 {
    let result = driver_init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());

    return result;
}
