#![no_main] // <1>
#![feature(alloc_error_handler)]
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

use core::panic::PanicInfo;

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

extern "C" {
    fn lookup_kallsyms(addr: u64, level: i32) -> i32;
}

// 声明全局的分配器
#[cfg_attr(not(test), global_allocator)]
pub static KERNEL_ALLOCATOR: KernelAllocator = KernelAllocator;

/// 全局的panic处理函数
///
/// How to use unwinding lib:
///
/// ```
/// pub fn test_unwind() {
///    struct UnwindTest;
///    impl Drop for UnwindTest {
///        fn drop(&mut self) {
///            println!("Drop UnwindTest");
///        }
///    }
///    let res1 = unwinding::panic::catch_unwind(|| {
///        let _unwind_test = UnwindTest;
///        println!("Test panic...");
///        panic!("Test panic");
///    });
///    assert_eq!(res1.is_err(), true);
///    let res2 = unwinding::panic::catch_unwind(|| {
///        let _unwind_test = UnwindTest;
///        println!("Test no panic...");
///        0
///    });
///    assert_eq!(res2.is_ok(), true);
/// }
/// ```
///
#[cfg(target_os = "none")]
#[panic_handler]
#[no_mangle]
pub fn panic(info: &PanicInfo) -> ! {
    use log::error;

    error!("Kernel Panic Occurred.");

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
    println!("Message:\n\t{}", info.message());
    #[cfg(feature = "backtrace")]
    {
        let mut data = hook::CallbackData { counter: 0 };
        println!("Rust Panic Backtrace:");
        let res = unwinding::panic::begin_panic_with_hook::<hook::Tracer>(
            alloc::boxed::Box::new(()),
            &mut data,
        );
        log::error!("panic unreachable: {:?}", res.0);
    }
    println!(
        "Current PCB:\n\t{:?}",
        process::ProcessManager::current_pcb()
    );
    process::ProcessManager::exit(usize::MAX);
}
#[cfg(feature = "backtrace")]
mod hook {
    use crate::lookup_kallsyms;
    use unwinding::abi::{UnwindContext, UnwindReasonCode, _Unwind_GetIP};
    use unwinding::panic::UserUnwindTrace;

    /// User hook for unwinding
    ///
    /// During stack backtrace, the user can print the function location of the current stack frame.
    pub struct Tracer;
    pub struct CallbackData {
        pub counter: usize,
    }
    impl UserUnwindTrace for Tracer {
        type Arg = CallbackData;

        fn trace(ctx: &UnwindContext<'_>, arg: *mut Self::Arg) -> UnwindReasonCode {
            let data = unsafe { &mut *(arg) };
            data.counter += 1;
            let pc = _Unwind_GetIP(ctx);
            unsafe {
                lookup_kallsyms(pc as u64, data.counter as i32);
            }
            UnwindReasonCode::NO_REASON
        }
    }
}
