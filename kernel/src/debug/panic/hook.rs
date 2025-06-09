use crate::libs::spinlock::SpinLock;

use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(not(target_arch = "loongarch64"))]
    {
        use unwinding::abi::{UnwindContext, UnwindReasonCode, _Unwind_Backtrace, _Unwind_GetIP};
        use core::ffi::c_void;
        use crate::debug::traceback::lookup_kallsyms;
    }
}

static GLOBAL_LOCK: SpinLock<()> = SpinLock::new(());

#[cfg(target_arch = "loongarch64")]
pub fn print_stack_trace() {
    let _lock = GLOBAL_LOCK.lock();
    println!("This Arch does not support stack trace printing.");
}

#[cfg(not(target_arch = "loongarch64"))]
pub fn print_stack_trace() {
    let _lock = GLOBAL_LOCK.lock();
    println!("Rust Panic Backtrace:");
    struct CallbackData {
        counter: usize,
        kernel_main: bool,
    }
    extern "C" fn callback(unwind_ctx: &UnwindContext<'_>, arg: *mut c_void) -> UnwindReasonCode {
        let data = unsafe { &mut *(arg as *mut CallbackData) };
        if data.kernel_main {
            // If we are in kernel_main, we don't need to print the backtrace.
            return UnwindReasonCode::NORMAL_STOP;
        }
        data.counter += 1;
        let pc = _Unwind_GetIP(unwind_ctx);
        if pc > 0 {
            let is_kernel_main = unsafe { lookup_kallsyms(pc as u64, data.counter as i32) };
            data.kernel_main = is_kernel_main;
        }
        UnwindReasonCode::NO_REASON
    }

    let mut data = CallbackData {
        counter: 0,
        kernel_main: false,
    };
    _Unwind_Backtrace(callback, &mut data as *mut _ as _);
}
