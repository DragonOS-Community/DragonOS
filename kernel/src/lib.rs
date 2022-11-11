#![no_std]                       // <1>
#![no_main]                      // <1>
#![feature(core_intrinsics)]     // <2>
#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]

use core::ffi::c_char;
use core::intrinsics;            // <2>
use core::panic::PanicInfo;      // <3>
include!("include/bindings/bindings.rs");

#[panic_handler]
#[no_mangle]
pub fn panic(_info: &PanicInfo) -> ! {
  intrinsics::abort();           // <4>
}

#[no_mangle]
pub extern "C" fn __rust_demo_func() -> i32 {
  unsafe{
    let f = b"\nDragonOS's Rust lib called printk_color()\n".as_ptr() as *const c_char;
    printk_color(GREEN, BLACK, f);
  }
  return 0;
}
