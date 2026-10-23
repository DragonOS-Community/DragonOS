pub mod hook;
use alloc::boxed::Box;
use cfg_if::cfg_if;
use core::hint::spin_loop;

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

const MAX_PANIC_COUNT: u8 = 2;

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
pub fn panic(info: &PanicInfo) -> ! {
    PANIC_COUNTER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    error!(
        "Kernel Panic Occurred. raw_pid: {}",
        process::ProcessManager::current_pid().data()
    );

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
    if PANIC_COUNTER.load(core::sync::atomic::Ordering::Relaxed) > MAX_PANIC_COUNT {
        println!(
            "Panic Counter: {}, too many panics, halt.",
            PANIC_COUNTER.load(core::sync::atomic::Ordering::Relaxed)
        );
        loop {}
    }

    #[cfg(not(target_arch = "loongarch64"))]
    if info.can_unwind() {
        let guard = Box::new(PanicGuard::new());
        hook::print_stack_trace();
        let _res = unwinding::panic::begin_panic(guard);
        // log::error!("panic unreachable: {:?}", _res.0);
    }
    if process::ProcessManager::initialized() {
        let current_pid = process::ProcessManager::current_pid();
        if current_pid.data() == 0 {
            println!("Current PCB:\n\t<idle task pid=0, cannot exit>");
            // 对齐 Linux 的 panic 语义：出事的 CPU 必须发布自己的终态并停止
            // 再取中断，不能留下一个"IF=1 空转"的僵尸核。
            //
            // 僵尸核的危害：它不再执行任何内存访问，因此本次 shootdown 对它
            // 已无意义，但只要它仍被视作 Online，其它核发来的 TLB IPI 就永远
            // 等不到 ack（发起核在关抢占的等待环里），局部故障直接放大成整机
            // 死锁。`stop_this_cpu()` 会走 Online→Dying→Dead，把本核从 RCU 与
            // shootdown 目标集中摘除，然后 halt。
            #[cfg(target_arch = "x86_64")]
            crate::arch::process::stop_this_cpu();
            #[cfg(not(target_arch = "x86_64"))]
            loop {
                spin_loop();
            }
        }

        println!(
            "Current PCB:\n\t{:?}",
            process::ProcessManager::current_pcb()
        );
        process::ProcessManager::exit(usize::MAX);
    } else {
        println!("Current PCB:\n\t<unavailable before process manager init>");
        loop {
            spin_loop();
        }
    }
}

/// The wrapper of `unwinding::panic::begin_panic`. If the panic is
/// caught, it will return the result of the function.
/// If the panic is not caught, it will return an error.
#[cfg(not(target_arch = "loongarch64"))]
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

#[cfg(not(target_arch = "loongarch64"))]
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
