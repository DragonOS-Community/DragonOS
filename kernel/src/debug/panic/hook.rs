use crate::debug::traceback::lookup_kallsyms;
use crate::libs::spinlock::SpinLock;
use core::ffi::c_void;
use unwinding::abi::{UnwindContext, UnwindReasonCode, _Unwind_Backtrace, _Unwind_GetIP};

pub fn print_stack_trace() {
    static GLOBAL_LOCK: SpinLock<()> = SpinLock::new(());
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
