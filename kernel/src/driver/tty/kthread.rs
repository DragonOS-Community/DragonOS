//! tty刷新内核线程

use alloc::{string::ToString, sync::Arc};
use kdepends::thingbuf::StaticThingBuf;

use crate::{
    arch::CurrentIrqArch,
    driver::tty::virtual_terminal::vc_manager,
    exception::InterruptArch,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{schedule, SchedMode},
};

/// 用于缓存键盘输入的缓冲区
static KEYBUF: StaticThingBuf<u8, 512> = StaticThingBuf::new();

static mut TTY_REFRESH_THREAD: Option<Arc<ProcessControlBlock>> = None;

pub(super) fn tty_flush_thread_init() {
    let closure =
        KernelThreadClosure::StaticEmptyClosure((&(tty_refresh_thread as fn() -> i32), ()));
    let pcb = KernelThreadMechanism::create_and_run(closure, "tty_refresh".to_string())
        .ok_or("")
        .expect("create tty_refresh thread failed");
    unsafe {
        TTY_REFRESH_THREAD = Some(pcb);
    }
}

fn tty_refresh_thread() -> i32 {
    const TO_DEQUEUE_MAX: usize = 256;
    loop {
        if KEYBUF.is_empty() {
            // 如果缓冲区为空，就休眠
            let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            ProcessManager::mark_sleep(true).expect("TTY_REFRESH_THREAD can not mark sleep");
            schedule(SchedMode::SM_NONE);
        }

        let to_dequeue = core::cmp::min(KEYBUF.len(), TO_DEQUEUE_MAX);
        if to_dequeue == 0 {
            continue;
        }
        let mut data = [0u8; TO_DEQUEUE_MAX];
        for item in data.iter_mut().take(to_dequeue) {
            *item = KEYBUF.pop().unwrap();
        }

        if let Some(cur_vc) = vc_manager().current_vc() {
            let _ = cur_vc
                .port()
                .receive_buf(&data[0..to_dequeue], &[], to_dequeue);
        } else {
            // 这里由于stdio未初始化，所以无法找到port
            // TODO: 考虑改用双端队列，能够将丢失的输入插回
        }
    }
}

/// 发送数据到tty刷新线程
pub fn send_to_tty_refresh_thread(data: &[u8]) {
    if unsafe { TTY_REFRESH_THREAD.is_none() } {
        return;
    }

    for item in data {
        KEYBUF.push(*item).ok();
    }
    let _ = ProcessManager::wakeup(unsafe { TTY_REFRESH_THREAD.as_ref().unwrap() });
}
