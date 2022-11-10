#![no_std]                       // <1>
#![no_main]                      // <1>
#![feature(core_intrinsics)]     // <2>

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
pub extern "C" fn eestart() -> i32 {
  unsafe{
    let f = b"\nDragonOS's Rust lib called printk_color()\n".as_ptr() as *const c_char;
    printk_color(GREEN, BLACK, f);
  }
  return 0;
}
