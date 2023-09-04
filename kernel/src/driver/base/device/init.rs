use crate::{driver::uart::uart_device::uart_init, syscall::SystemError};

pub extern "C" fn device_init() -> i32{
    let result = uart_device_init();
    if result.is_err(){
    return result.unwrap_err().to_posix_errno();
    }
    return 0;
}

fn uart_device_init() -> Result<(), SystemError>{
    
    return uart_init();
}