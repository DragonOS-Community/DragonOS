#![no_std] // <1>
#![no_main] // <1>
#![feature(alloc_error_handler)]
#![feature(allocator_api)]
#![feature(arbitrary_self_types)]
#![feature(asm_const)]
#![feature(const_mut_refs)]
#![feature(core_intrinsics)]
#![feature(c_void_variant)]
#![feature(drain_filter)]
#![feature(is_some_and)]
#![feature(naked_functions)]
#![feature(panic_info_message)]
#![feature(ptr_internals)]
#![feature(trait_upcasting)]
#![feature(slice_ptr_get)]
#![feature(vec_into_raw_parts)]

#[allow(non_upper_case_globals)]
#[allow(non_camel_case_types)]
#[allow(non_snake_case)]
use core::panic::PanicInfo;

/// 导出x86_64架构相关的代码，命名为arch模块
#[macro_use]
mod arch;
#[macro_use]
mod libs;
#[macro_use]
mod include;
mod driver; // 如果driver依赖了libs，应该在libs后面导出
mod exception;
mod filesystem;
mod ipc;
mod mm;
mod net;
mod process;
mod sched;
mod smp;
mod syscall;
mod time;

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate bitflags;
extern crate elf;
#[macro_use]
extern crate lazy_static;
extern crate memoffset;
extern crate num;
#[macro_use]
extern crate num_derive;
extern crate smoltcp;
extern crate thingbuf;
#[cfg(target_arch = "x86_64")]
extern crate x86;

use crate::libs::lib_ui::textui::FontColor;
use crate::mm::allocator::kernel_allocator::KernelAllocator;

use crate::process::ProcessManager;
// <3>
use crate::{
    arch::asm::current::current_pcb,
    include::bindings::bindings::{BLACK, GREEN},
    net::net_core::net_init,
};

// 声明全局的分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator;

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

    println!("Current PCB:\n\t{:?}", *(ProcessManager::current_pcb()));

    ProcessManager::exit(usize::MAX);
}

/// 该函数用作测试，在process.c的initial_kernel_thread()中调用了此函数
#[no_mangle]
pub extern "C" fn __rust_demo_func() -> i32 {
    printk_color!(FontColor::GREEN, FontColor::BLACK, "__rust_demo_func()\n");
    let r = net_init();
    if r.is_err() {
        kwarn!("net_init() failed: {:?}", r.err().unwrap());
    }
    return 0;
}
