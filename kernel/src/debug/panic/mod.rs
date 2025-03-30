mod hook;
use alloc::boxed::Box;
use cfg_if::cfg_if;

use log::error;

use crate::process;
use system_error::SystemError;

cfg_if! {
    if #[cfg(target_os = "none")] {
        use core::panic::PanicInfo;
        use core::sync::atomic::AtomicU8;

        static PANIC_COUNTER: AtomicU8 = AtomicU8::new(0);
    }
}

#[derive(Debug)]
struct PanicGuard;

impl PanicGuard {
    pub fn new() -> Self {
        crate::arch::panic_pre_work();
        Self
    }
}

impl Drop for PanicGuard {
    fn drop(&mut self) {
        crate::arch::panic_post_work();
    }
}

/// 全局的panic处理函数
///
#[cfg(target_os = "none")]
#[panic_handler]
#[no_mangle]
pub fn panic(info: &PanicInfo) -> ! {
    PANIC_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
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
    if PANIC_COUNTER.load(core::sync::atomic::Ordering::Relaxed) > 8 {
        println!(
            "Panic Counter: {}, too many panics, halt.",
            PANIC_COUNTER.load(core::sync::atomic::Ordering::Relaxed)
        );
        loop {}
    }

    if info.can_unwind() {
        let guard = Box::new(PanicGuard::new());
        hook::print_stack_trace();
        let _res = unwinding::panic::begin_panic(guard);
        // log::error!("panic unreachable: {:?}", _res.0);
    }
    println!(
        "Current PCB:\n\t{:?}",
        process::ProcessManager::current_pcb()
    );
    process::ProcessManager::exit(usize::MAX);
}

/// The wrapper of `unwinding::panic::begin_panic`. If the panic is
/// caught, it will return the result of the function.
/// If the panic is not caught, it will return an error.
pub fn kernel_catch_unwind<R, F: FnOnce() -> R>(f: F) -> Result<R, SystemError> {
    let res = unwinding::panic::catch_unwind(f);
    match res {
        Ok(r) => Ok(r),
        Err(e) => {
            log::error!("Catch Unwind Error: {:?}", e);
            Err(SystemError::MAXERRNO)
        }
    }
}

#[allow(unused)]
pub fn test_unwind() {
    struct UnwindTest;
    impl Drop for UnwindTest {
        fn drop(&mut self) {
            log::info!("Drop UnwindTest");
        }
    }
    log::error!("Test unwind");
    let res1 = unwinding::panic::catch_unwind(|| {
        let _unwind_test = UnwindTest;
        log::error!("Test panic...");
        panic!("Test panic");
    });
    assert!(res1.is_err());
    let res2 = unwinding::panic::catch_unwind(|| {
        let _unwind_test = UnwindTest;
        log::error!("Test no panic...");
        0
    });
    assert!(res2.is_ok());
}
