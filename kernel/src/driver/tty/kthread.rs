//! tty刷新内核线程

use alloc::{string::ToString, sync::Arc};
use kdepends::thingbuf::StaticThingBuf;

use crate::{
    arch::CurrentIrqArch,
    driver::tty::virtual_terminal::vc_manager,
    exception::tasklet::{tasklet_schedule, Tasklet, TaskletData},
    exception::InterruptArch,
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::{schedule, SchedMode},
};

/// 用于缓存 IRQ 上下文投递的 TTY 输入。
///
/// N_TTY 规范模式按 Linux 语义支持 4096 字节行缓冲；这里的 IRQ 暂存队列必须
/// 大于该长度，避免输入还没进入行规程就被提前截断。
const TTY_RX_IRQ_BUF_SIZE: usize = 8192;

/// 单次从 IRQ 暂存队列投递给行规程的最大字节数。
const TTY_RX_DEQUEUE_MAX: usize = 256;

static KEYBUF: StaticThingBuf<u8, TTY_RX_IRQ_BUF_SIZE> = StaticThingBuf::new();

static mut TTY_REFRESH_THREAD: Option<Arc<ProcessControlBlock>> = None;

lazy_static! {
    /// TTY RX tasklet，用于在 softirq 上下文中处理 TTY 输入
    static ref TTY_RX_TASKLET: Arc<Tasklet> = Tasklet::new(tty_rx_tasklet_fn, 0, None);
}

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
    loop {
        if KEYBUF.is_empty() {
            // 如果缓冲区为空，就休眠
            let _guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            ProcessManager::mark_sleep(true).expect("TTY_REFRESH_THREAD can not mark sleep");
            schedule(SchedMode::SM_NONE);
        }

        let to_dequeue = core::cmp::min(KEYBUF.len(), TTY_RX_DEQUEUE_MAX);
        if to_dequeue == 0 {
            continue;
        }
        let mut data = [0u8; TTY_RX_DEQUEUE_MAX];
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

fn tty_rx_tasklet_fn(_data: usize, _data_obj: Option<Arc<dyn TaskletData>>) {
    // 在 softirq/tasklet 上下文：不做 drain，只负责唤醒线程去处理输入。
    if unsafe { TTY_REFRESH_THREAD.is_none() } {
        return;
    }
    if KEYBUF.is_empty() {
        return;
    }
    let _ = ProcessManager::wakeup(unsafe { TTY_REFRESH_THREAD.as_ref().unwrap() });
}

/// 在 hardirq 上下文投递输入：只入队并调度 tasklet（不直接唤醒线程）。
///
/// 这样可以避免在硬中断里触碰调度/唤醒逻辑，符合 Linux bottom-half 语义。
pub fn enqueue_tty_rx_from_irq(data: &[u8]) {
    if unsafe { TTY_REFRESH_THREAD.is_none() } {
        return;
    }
    for item in data {
        KEYBUF.push(*item).ok();
    }
    tasklet_schedule(&TTY_RX_TASKLET);
}
