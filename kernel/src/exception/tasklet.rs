//! Tasklet：一种基于 softirq 的 bottom-half 机制（Linux 风格，Rust 友好）
//!
//! 语义（MVP）：
//! - 可以在 hardirq/task context 调用 `tasklet_schedule()`。
//! - 同一个 tasklet 在同一时间不会并发执行（自串行）。
//! - 重复 schedule 不会导致无限入队（使用 `is_scheduled` 去重）。
//! - tasklet 在 softirq 上下文执行：不允许睡眠。

use core::sync::atomic::{AtomicBool, Ordering};

use alloc::{sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    exception::softirq::{softirq_vectors, SoftirqNumber, SoftirqVec},
    libs::spinlock::SpinLock,
    mm::percpu::{PerCpu, PerCpuVar},
};

#[derive(Debug)]
pub struct Tasklet {
    is_scheduled: AtomicBool,
    is_running: AtomicBool,
    callback: fn(usize),
    data: usize,
}

impl Tasklet {
    pub fn new(callback: fn(usize), data: usize) -> Arc<Self> {
        Arc::new(Self {
            is_scheduled: AtomicBool::new(false),
            is_running: AtomicBool::new(false),
            callback,
            data,
        })
    }
}

lazy_static! {
    /// 每个 CPU 的 tasklet 队列
    static ref TASKLET_QUEUE: PerCpuVar<SpinLock<Vec<Arc<Tasklet>>>> = {
        let mut v = Vec::with_capacity(PerCpu::MAX_CPU_NUM as usize);
        v.resize_with(PerCpu::MAX_CPU_NUM as usize, || SpinLock::new(Vec::new()));
        PerCpuVar::new(v).expect("PerCpuVar length mismatch")
    };
}

#[inline(always)]
fn local_queue() -> &'static SpinLock<Vec<Arc<Tasklet>>> {
    TASKLET_QUEUE.get()
}

/// 调度一个 tasklet（去重）。
///
/// 允许在 hardirq/softirq/task context 调用；不会睡眠。
pub fn tasklet_schedule(t: &Arc<Tasklet>) {
    if t.is_scheduled
        .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        return;
    }

    // 入队时关本地中断，避免与 softirq handler 并发访问队列。
    local_queue().lock_irqsave().push(t.clone());
    softirq_vectors().raise_softirq(SoftirqNumber::TASKLET);
}

#[derive(Debug, Default)]
struct TaskletSoftirq;

impl SoftirqVec for TaskletSoftirq {
    fn run(&self) {
        // 将队列快速取出到本地，缩短关中断时间。
        let mut processing = {
            let mut q = local_queue().lock_irqsave();
            core::mem::take(&mut *q)
        };

        while let Some(t) = processing.pop() {
            // 若正在运行，则把它丢回队列并重新 raise（保证"自串行"，但允许再次被调度）。
            if t.is_running
                .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
                .is_err()
            {
                local_queue().lock_irqsave().push(t);
                softirq_vectors().raise_softirq(SoftirqNumber::TASKLET);
                continue;
            }

            // 允许回调里再次 schedule（此时可重新入队）。
            // 注意：即使 tasklet 被 disable，已经入队的 tasklet 仍然会执行
            // （符合 Linux 语义：disable 只阻止新的 schedule，不阻止已入队的执行）。
            t.is_scheduled.store(false, Ordering::Release);

            (t.callback)(t.data);

            t.is_running.store(false, Ordering::Release);
        }
    }
}

/// 初始化 tasklet 子系统：注册 TASKLET softirq handler。
///
/// TASKLET_QUEUE 使用 lazy_static 自动初始化，无需手动初始化。
#[inline(never)]
pub fn tasklet_init() -> Result<(), SystemError> {
    let handler = Arc::new(TaskletSoftirq);
    softirq_vectors().register_softirq(SoftirqNumber::TASKLET, handler)?;
    Ok(())
}
