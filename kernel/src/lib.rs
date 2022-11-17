#![no_std] // <1>
#![no_main] // <1>
#![feature(core_intrinsics)] // <2>
#![feature(alloc_error_handler)]

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]

use core::intrinsics; // <2>
use core::panic::PanicInfo;


#[macro_use]
mod mm;
mod include;
mod libs;
mod ipc;

extern crate alloc;

use mm::allocator::KernelAllocator;

// <3>
use crate::include::bindings::bindings::{BLACK, GREEN};

// 声明全局的slab分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator {};

/// 全局的panic处理函数
#[panic_handler]
#[no_mangle]
pub fn panic(_info: &PanicInfo) -> ! {
    intrinsics::abort(); // <4>
}

/// 该函数用作测试，在process.c的initial_kernel_thread()中调用了此函数
#[no_mangle]
pub extern "C" fn __rust_demo_func() -> i32 {
    
    printk_color!(GREEN, BLACK, "__rust_demo_func()\n");

    return 0;
}
