use crate::{driver::uart::uart_device::uart_init, kinfo, syscall::SystemError};

#[no_mangle]
pub extern "C" fn rs_device_init() -> i32 {
    let result = device_init()
        .map(|_| 0)
        .unwrap_or_else(|e| e.to_posix_errno());

    return result;
}

pub fn device_init() -> Result<(), SystemError> {
    uart_init()?;
    kinfo!("device init success");
    return Ok(());
}
