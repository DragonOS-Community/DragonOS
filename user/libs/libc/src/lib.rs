#![no_std] // <1>
#![no_main] // <1>
#![feature(core_intrinsics)] // <2>
#![feature(alloc_error_handler)]
#![feature(panic_info_message)]

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
use core::panic::PanicInfo;

use include::internal::bindings::bindings::putchar;

mod include;

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

#[no_mangle]
pub extern "C" fn scanf() {
    loop {
        unsafe {
            putchar(88);
        }
    }
}
