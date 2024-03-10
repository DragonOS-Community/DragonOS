#![no_main] // <1>
#![feature(alloc_error_handler)]
#![feature(allocator_api)]
#![feature(arbitrary_self_types)]
#![feature(asm_const)]
#![feature(concat_idents)]
#![feature(const_for)]
#![feature(const_mut_refs)]
#![feature(const_trait_impl)]
#![feature(const_transmute_copy)]
#![feature(const_refs_to_cell)]
#![feature(core_intrinsics)]
#![feature(c_void_variant)]
#![feature(extract_if)]
#![feature(fn_align)]
#![feature(inline_const)]
#![feature(naked_functions)]
#![feature(new_uninit)]
#![feature(panic_info_message)]
#![feature(ptr_internals)]
#![feature(ptr_to_from_bits)]
#![feature(trait_upcasting)]
#![feature(slice_ptr_get)]
#![feature(vec_into_raw_parts)]
#![cfg_attr(target_os = "none", no_std)]
// clippy的配置
#![deny(clippy::all)]
// DragonOS允许在函数中使用return语句（尤其是长函数时，我们推荐这么做）
#![allow(clippy::let_and_return)]
#![allow(clippy::needless_pass_by_ref_mut)]
#![allow(clippy::needless_return)]
#![allow(clippy::upper_case_acronyms)]

#[cfg(test)]
#[macro_use]
extern crate std;

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
mod debug;
mod driver; // 如果driver依赖了libs，应该在libs后面导出
mod exception;
mod filesystem;
mod init;
mod ipc;
mod misc;
mod mm;
mod net;
mod process;
mod sched;
mod smp;
mod syscall;
mod time;

#[cfg(target_arch = "x86_64")]
mod virt;

#[macro_use]
extern crate alloc;
#[macro_use]
extern crate atomic_enum;
#[macro_use]
extern crate bitflags;
extern crate elf;
#[macro_use]
extern crate lazy_static;
extern crate num;
#[macro_use]
extern crate num_derive;
extern crate smoltcp;
#[macro_use]
extern crate intertrait;
#[cfg(target_arch = "x86_64")]
extern crate x86;

extern crate klog_types;
extern crate uefi;
extern crate uefi_raw;

use crate::mm::allocator::kernel_allocator::KernelAllocator;

use crate::process::ProcessManager;

#[cfg(all(feature = "backtrace", target_arch = "x86_64"))]
extern crate mini_backtrace;

extern "C" {
    fn lookup_kallsyms(addr: u64, level: i32) -> i32;
}

// 声明全局的分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator;

/// 全局的panic处理函数
#[cfg(target_os = "none")]
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

    #[cfg(all(feature = "backtrace", target_arch = "x86_64"))]
    {
        unsafe {
            let bt = mini_backtrace::Backtrace::<16>::capture();
            println!("Rust Panic Backtrace:");
            let mut level = 0;
            for frame in bt.frames {
                lookup_kallsyms(frame as u64, level);
                level += 1;
            }
        };
    }

    println!("Current PCB:\n\t{:?}", *(ProcessManager::current_pcb()));
    ProcessManager::exit(usize::MAX);
}
