use crate::debug::traceback::lookup_kallsyms;
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
