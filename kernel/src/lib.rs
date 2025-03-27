#![no_main] // <1>
#![feature(alloc_error_handler)]
#![feature(new_zeroed_alloc)]
#![feature(allocator_api)]
#![feature(arbitrary_self_types)]
#![feature(concat_idents)]
#![feature(const_for)]
#![feature(const_trait_impl)]
#![feature(core_intrinsics)]
#![feature(c_void_variant)]
#![feature(extract_if)]
#![feature(fn_align)]
#![feature(linked_list_retain)]
#![feature(naked_functions)]
#![feature(ptr_internals)]
#![feature(trait_upcasting)]
#![feature(slice_ptr_get)]
#![feature(sync_unsafe_cell)]
#![feature(vec_into_raw_parts)]
#![feature(c_variadic)]
#![feature(asm_goto)]
#![feature(linkage)]
#![cfg_attr(target_os = "none", no_std)]
#![allow(static_mut_refs, non_local_definitions, internal_features)]
// clippy的配置
#![deny(clippy::all)]
// DragonOS允许在函数中使用return语句（尤其是长函数时，我们推荐这么做）
#![allow(
    clippy::macro_metavars_in_unsafe,
    clippy::upper_case_acronyms,
    clippy::single_char_pattern,
    clippy::needless_return,
    clippy::needless_pass_by_ref_mut,
    clippy::let_and_return,
    clippy::bad_bit_mask
)]

#[cfg(test)]
#[macro_use]
extern crate std;

/// 导出x86_64架构相关的代码，命名为arch模块
#[macro_use]
mod arch;
#[macro_use]
mod libs;
#[macro_use]
mod include;
mod bpf;
mod cgroup;
mod debug;
mod driver; // 如果driver依赖了libs，应该在libs后面导出
mod exception;
mod filesystem;
mod init;
mod ipc;
mod misc;
mod mm;
mod namespaces;
mod net;
mod perf;
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
#[macro_use]
extern crate kcmdline_macros;
extern crate klog_types;
extern crate uefi;
extern crate uefi_raw;
#[macro_use]
extern crate wait_queue_macros;

use crate::mm::allocator::kernel_allocator::KernelAllocator;

// 声明全局的分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator;
