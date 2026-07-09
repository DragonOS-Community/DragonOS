//! tty刷新内核线程

use alloc::{string::ToString, sync::Arc};

use crate::{
    driver::{
        char::virtio_console::retry_virtio_console_input,
        serial::serial8250::retry_serial8250_input,
        tty::{
            tty_core::TtyCore,
            tty_port::{TtyInputByteResult, TTY_PORT_RX_CHUNK_SIZE},
            virtual_terminal::{vc_manager, MAX_NR_CONSOLES},
        },
    },
    exception::tasklet::{tasklet_schedule, Tasklet, TaskletData},
    libs::wait_queue::WaitQueue,
    process::kthread::{KernelThreadClosure, KernelThreadMechanism},
    sched::{schedule, SchedMode},
};

const TTY_INPUT_WORK_QUEUE_SIZE: usize = MAX_NR_CONSOLES as usize;
const TTY_INPUT_DRAIN_BUDGET: usize = 16;

struct TtyInputWorkQueue {
    queue: [usize; TTY_INPUT_WORK_QUEUE_SIZE],
    in_queue: [bool; TTY_INPUT_WORK_QUEUE_SIZE],
    head: usize,
    len: usize,
}

impl TtyInputWorkQueue {
    const fn new() -> Self {
        Self {
            queue: [0; TTY_INPUT_WORK_QUEUE_SIZE],
            in_queue: [false; TTY_INPUT_WORK_QUEUE_SIZE],
            head: 0,
            len: 0,
        }
    }

    fn push(&mut self, vc_index: usize) -> bool {
        if vc_index >= TTY_INPUT_WORK_QUEUE_SIZE || self.in_queue[vc_index] {
            return false;
        }
        debug_assert!(self.len < TTY_INPUT_WORK_QUEUE_SIZE);
        if self.len >= TTY_INPUT_WORK_QUEUE_SIZE {
            return false;
        }
        let idx = (self.head + self.len) % TTY_INPUT_WORK_QUEUE_SIZE;
        self.queue[idx] = vc_index;
        self.in_queue[vc_index] = true;
        self.len += 1;
        true
    }

    fn pop(&mut self) -> Option<usize> {
        if self.len == 0 {
            return None;
        }
        let vc_index = self.queue[self.head];
        self.head = (self.head + 1) % TTY_INPUT_WORK_QUEUE_SIZE;
        self.len -= 1;
        self.in_queue[vc_index] = false;
        if self.len == 0 {
            self.head = 0;
        }
        Some(vc_index)
    }
}

static TTY_INPUT_WORK_QUEUE: crate::libs::spinlock::SpinLock<TtyInputWorkQueue> =
    crate::libs::spinlock::SpinLock::new(TtyInputWorkQueue::new());
static TTY_REFRESH_WAIT_QUEUE: WaitQueue = WaitQueue::default();

lazy_static! {
    /// TTY RX tasklet that wakes the TTY input thread from softirq context.
    static ref TTY_RX_TASKLET: Arc<Tasklet> = Tasklet::new(tty_rx_tasklet_fn, 0, None);
}

pub(super) fn tty_flush_thread_init() {
    let closure =
        KernelThreadClosure::StaticEmptyClosure((&(tty_refresh_thread as fn() -> i32), ()));
    KernelThreadMechanism::create_and_run(closure, "tty_refresh".to_string())
        .ok_or("")
        .expect("create tty_refresh thread failed");
}

fn tty_refresh_thread() -> i32 {
    loop {
        let vc_index =
            TTY_REFRESH_WAIT_QUEUE.wait_until(|| TTY_INPUT_WORK_QUEUE.lock_irqsave().pop());
        drain_vc_input(vc_index);
    }
}

fn drain_vc_input(vc_index: usize) {
    let Some(vc) = vc_manager().get(vc_index) else {
        return;
    };
    let port = vc.port();
    let mut budget = TTY_INPUT_DRAIN_BUDGET;

    while budget != 0 {
        let drain = match port.drain_input_to_ldisc(TTY_PORT_RX_CHUNK_SIZE) {
            Ok(drain) => drain,
            Err(err) => {
                log::warn!("tty_refresh: drain vc{} input failed: {:?}", vc_index, err);
                break;
            }
        };

        if drain.freed_room != 0 {
            retry_tty_input_producers();
        }

        if drain.copied == 0 || !drain.still_pending {
            break;
        }

        if drain.blocked {
            break;
        }

        budget -= 1;
        if budget == 0 {
            queue_tty_input_work(vc_index);
            schedule(SchedMode::SM_NONE);
            break;
        }
    }
}

fn tty_rx_tasklet_fn(_data: usize, _data_obj: Option<Arc<dyn TaskletData>>) {
    TTY_REFRESH_WAIT_QUEUE.wakeup(None);
}

fn queue_tty_input_work_common(vc_index: usize) -> bool {
    TTY_INPUT_WORK_QUEUE.lock_irqsave().push(vc_index)
}

fn queue_tty_input_work_from_irq(vc_index: usize) {
    if queue_tty_input_work_common(vc_index) {
        tasklet_schedule(&TTY_RX_TASKLET);
    }
}

fn queue_tty_input_work(vc_index: usize) {
    if queue_tty_input_work_common(vc_index) {
        TTY_REFRESH_WAIT_QUEUE.wakeup(None);
    }
}

/// 在 hardirq 上下文投递输入：只入队并调度 tasklet（不直接唤醒线程）。
pub fn enqueue_tty_rx_from_irq(data: &[u8]) -> usize {
    vc_manager()
        .current_vc_index()
        .map(|vc_index| enqueue_tty_rx_to_vc_from_irq(vc_index, data))
        .unwrap_or(0)
}

pub fn enqueue_tty_rx_to_vc_from_irq(vc_index: usize, data: &[u8]) -> usize {
    let Some(vc) = vc_manager().get(vc_index) else {
        return 0;
    };
    let accepted = vc.port().enqueue_input(data);
    if accepted != 0 {
        queue_tty_input_work_from_irq(vc_index);
    }
    accepted
}

pub fn enqueue_tty_rx_byte_to_vc_from_irq(
    vc_index: usize,
    producer: &mut dyn FnMut() -> Option<u8>,
) -> TtyInputByteResult {
    let Some(vc) = vc_manager().get(vc_index) else {
        return TtyInputByteResult::NoTarget;
    };
    let result = vc.port().enqueue_input_byte_with(producer);
    if result == TtyInputByteResult::Enqueued {
        queue_tty_input_work_from_irq(vc_index);
    }
    result
}

pub fn tty_port_input_room(vc_index: usize) -> usize {
    vc_manager()
        .get(vc_index)
        .map(|vc| vc.port().input_room())
        .unwrap_or(0)
}

pub fn retry_tty_input_producers() {
    retry_virtio_console_input();
    retry_serial8250_input();
}

pub fn tty_kick_input_worker(tty: Arc<TtyCore>) {
    let Some(vc_index) = tty.core().vc_index() else {
        retry_tty_input_producers();
        return;
    };

    if vc_manager()
        .get(vc_index)
        .map(|vc| vc.port().has_input())
        .unwrap_or(false)
    {
        queue_tty_input_work(vc_index);
    }
    retry_tty_input_producers();
}
