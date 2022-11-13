#![no_std] // <1>
#![no_main] // <1>
#![feature(core_intrinsics)] // <2>
#![feature(alloc_error_handler)]

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]

use core::{ffi::c_char};
use core::intrinsics; // <2>
use core::panic::PanicInfo;

#[macro_use]
mod mm;
mod include;
mod libs;

extern crate alloc;


use mm::allocator::KernelAllocator;

// <3>
use crate::{
    include::bindings::bindings::{printk_color, BLACK, GREEN},
};

// 声明全局的slab分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator{};

#[panic_handler]
#[no_mangle]
pub fn panic(_info: &PanicInfo) -> ! {
    intrinsics::abort(); // <4>
}
fn x()
{
  print!("12345=0x{:X}", 255); 
}

#[no_mangle]
pub extern "C" fn __rust_demo_func() -> i32 {
    unsafe {
        let f = b"\nDragonOS's Rust lib called printk_color()\n\0".as_ptr() as *const c_char;
        printk_color(GREEN, BLACK, f);
    }
    printk_color!(GREEN, BLACK, "{}", 123);
    // 测试从slab获取内存的过程
    x();
    // PrintkWriter.__write_string("Test custom print!");

    return 0;
}
