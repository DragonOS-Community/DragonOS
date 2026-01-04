#![allow(dead_code)]

use alloc::{boxed::Box, string::String};

use crate::{
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessManager,
    },
    sched::prio::MAX_RT_PRIO,
    smp::cpu::ProcessorId,
    time::{sleep::nanosleep, Duration, PosixTimeSpec},
};

pub fn fifo_demo_init() {
    let closure: Box<dyn Fn() -> i32 + Send + Sync> = Box::new(move || {
        let pcb = ProcessManager::current_pcb();

        // 设置CPU亲和性为Core 0
        pcb.sched_info().set_on_cpu(Some(ProcessorId::new(0)));

        // 设置调度策略为FIFO，优先级为50
        ProcessManager::set_fifo_policy(&pcb, MAX_RT_PRIO - 50).expect("Failed to set FIFO policy");

        loop {
            log::info!("fifo is running");

            // 睡眠5秒
            let sleep_time = PosixTimeSpec::from(Duration::from_secs(5));
            let _ = nanosleep(sleep_time);
        }
    });

    let _ = KernelThreadMechanism::create_and_run(
        KernelThreadClosure::EmptyClosure((closure, ())),
        String::from("fifo_demo"),
    );
}
