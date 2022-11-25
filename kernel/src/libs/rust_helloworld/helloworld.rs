#![no_std]
#![no_main]

use core::panic::PanicInfo;

#[no_mangle]
pub extern "C" fn rust_helloworld_a_plus_b(a: i32, b: i32) -> i32 {
    return a+b;
}

/// 这个函数将在panic时被调用
#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    loop {}
}

// rustc --crate-type staticlib  --target x86_64-unknown-none -o helloworld.o helloworld.rs 