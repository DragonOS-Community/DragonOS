#![no_std] // <1>
#![no_main] // <1>
#![feature(core_intrinsics)] // <2>
#![feature(alloc_error_handler)]
#![feature(panic_info_message)]
#![feature(drain_filter)]// 允许Vec的drain_filter特性

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
use core::panic::PanicInfo;

#[macro_use]
mod arch;
#[macro_use]
mod include;
mod ipc;

#[macro_use]
mod libs;
mod mm;
mod process;
mod sched;
mod smp;
mod driver;
mod time;

extern crate alloc;

use mm::allocator::KernelAllocator;

// <3>
use crate::{include::bindings::bindings::{process_do_exit, BLACK, GREEN}, arch::x86_64::asm::current::current_pcb, time::timekeep::ktime_get_real_ns};

// 声明全局的slab分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator {};

/// 全局的panic处理函数
#[panic_handler]
#[no_mangle]
pub fn panic(info: &PanicInfo) -> ! {
    kerror!("Kernel Panic Occurred.");

    match info.location() {
        Some(loc) => {
            println!(
                "Location:\n\tFile: {}\n\tLine: {}, Column: {}",
                loc.file(),
                loc.line(),
                loc.column()
            );
        }
        None => {
            println!("No location info");
        }
    }

    match info.message() {
        Some(msg) => {
            println!("Message:\n\t{}", msg);
        }
        None => {
            println!("No panic message.");
        }
    }

    println!("Current PCB:\n\t{:?}", current_pcb());
    unsafe {
        process_do_exit(u64::MAX);
    };
    loop {
        
    }
}

/// 该函数用作测试，在process.c的initial_kernel_thread()中调用了此函数
#[no_mangle]
pub extern "C" fn __rust_demo_func() -> i32 {
    printk_color!(GREEN, BLACK, "__rust_demo_func()\n");
    return 0;
}
