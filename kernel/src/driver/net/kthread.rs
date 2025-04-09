use alloc::borrow::ToOwned;
use alloc::sync::Arc;
use unified_init::macros::unified_init;

use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::init::initcall::INITCALL_SUBSYS;
use crate::net::NET_DEVICES;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::{ProcessControlBlock, ProcessManager};
use crate::sched::{schedule, SchedMode};

static mut NET_POLL_THREAD: Option<Arc<ProcessControlBlock>> = None;

#[unified_init(INITCALL_SUBSYS)]
pub fn net_poll_init() -> Result<(), system_error::SystemError> {
    let closure = KernelThreadClosure::StaticEmptyClosure((&(net_poll_thread as fn() -> i32), ()));
    let pcb = KernelThreadMechanism::create_and_run(closure, "net_poll".to_owned())
        .ok_or("")
        .expect("create net_poll thread failed");
    log::info!("net_poll thread created");
    unsafe {
        NET_POLL_THREAD = Some(pcb);
    }
    return Ok(());
}

fn net_poll_thread() -> i32 {
    log::info!("net_poll thread started");
    loop {
        for (_, iface) in NET_DEVICES.read_irqsave().iter() {
            iface.poll();
        }
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(true).expect("clocksource_watchdog_kthread:mark sleep failed");
        drop(irq_guard);
        schedule(SchedMode::SM_NONE);
    }
}

/// 拉起线程
pub(super) fn wakeup_poll_thread() {
    if unsafe { NET_POLL_THREAD.is_none() } {
        return;
    }
    let _ = ProcessManager::wakeup(unsafe { NET_POLL_THREAD.as_ref().unwrap() });
}
