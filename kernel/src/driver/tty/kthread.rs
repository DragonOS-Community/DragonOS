//! tty刷新内核线程

use alloc::{string::ToString, sync::Arc};
use kdepends::thingbuf::StaticThingBuf;

use crate::{
    arch::sched::sched,
    driver::tty::virtual_terminal::virtual_console::CURRENT_VCNUM,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessFlags,
    },
};

use super::tty_port::current_tty_port;

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
            unsafe {
                TTY_REFRESH_THREAD
                    .as_ref()
                    .unwrap()
                    .flags()
                    .insert(ProcessFlags::NEED_SCHEDULE)
            };

            sched();
        }

        let to_dequeue = core::cmp::min(KEYBUF.len(), TO_DEQUEUE_MAX);
        if to_dequeue == 0 {
            continue;
        }
        let mut data = [0u8; TO_DEQUEUE_MAX];
        for item in data.iter_mut().take(to_dequeue) {
            *item = KEYBUF.pop().unwrap();
        }

        if CURRENT_VCNUM.load(core::sync::atomic::Ordering::SeqCst) != -1 {
            let _ = current_tty_port().receive_buf(&data[0..to_dequeue], &[], to_dequeue);
        } else {
            // 这里由于stdio未初始化，所以无法找到port
            // TODO: 考虑改用双端队列，能够将丢失的输入插回
        }
    }
}

/// 发送数据到tty刷新线程
pub fn send_to_tty_refresh_thread(data: &[u8]) {
    for item in data {
        KEYBUF.push(*item).ok();
    }
}
