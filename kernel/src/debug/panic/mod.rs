#[cfg(feature = "backtrace")]
mod hook;
use cfg_if::cfg_if;

cfg_if! {
    if #[cfg(target_os = "none")] {
        use core::panic::PanicInfo;
        use core::sync::atomic::AtomicU8;

        static PANIC_COUNTER: AtomicU8 = AtomicU8::new(0);
    }
}
/// 全局的panic处理函数
///
#[cfg(target_os = "none")]
#[panic_handler]
#[no_mangle]
pub fn panic(info: &PanicInfo) -> ! {
    use log::error;

    use crate::process;
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
    #[cfg(feature = "backtrace")]
    {
        let mut data = hook::CallbackData { counter: 0 };
        println!("Rust Panic Backtrace:");
        let _res = unwinding::panic::begin_panic_with_hook::<hook::Tracer>(
            alloc::boxed::Box::new(()),
            &mut data,
        );
        // log::error!("panic unreachable: {:?}", res.0);
    }
    println!(
        "Current PCB:\n\t{:?}",
        process::ProcessManager::current_pcb()
    );
    process::ProcessManager::exit(usize::MAX);
}
