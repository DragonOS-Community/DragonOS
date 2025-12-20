use core::sync::atomic::{AtomicU64, Ordering};

use alloc::sync::Arc;

use crate::libs::wait_queue::WaitQueue;

use super::{ProcessControlBlock, ProcessManager};

#[derive(Debug, Default)]
pub struct ProcessCpuTime {
    pub utime: AtomicU64,
    pub stime: AtomicU64,
    pub sum_exec_runtime: AtomicU64,
}

impl ProcessControlBlock {
    #[inline(always)]
    pub fn cputime_wait_queue(&self) -> &WaitQueue {
        &self.cputime_wait_queue
    }

    #[inline(always)]
    pub fn cputime(&self) -> Arc<ProcessCpuTime> {
        self.cpu_time.clone()
    }

    /// 当前线程（PCB）的 CPU 时间（ns），语义对齐 Linux 的 CLOCK_THREAD_CPUTIME_ID：user+system。
    #[inline]
    pub fn thread_cputime_ns(&self) -> u64 {
        let ct = self.cputime();
        ct.utime.load(Ordering::Relaxed) + ct.stime.load(Ordering::Relaxed)
    }

    /// 当前进程（线程组）的 CPU 时间（ns），语义对齐 Linux 的 CLOCK_PROCESS_CPUTIME_ID。
    ///
    /// 说明：目前通过遍历线程组成员并累加每线程的 user+system 得到。
    pub fn process_cputime_ns(&self) -> u64 {
        // 尽量选择线程组组长作为“进程”视角。
        let leader = if self.is_thread_group_leader() {
            self.self_ref
                .upgrade()
                .unwrap_or_else(ProcessManager::current_pcb)
        } else {
            self.threads_read_irqsave()
                .group_leader()
                .or_else(|| self.self_ref.upgrade())
                .unwrap_or_else(ProcessManager::current_pcb)
        };

        if !leader.is_thread_group_leader() {
            // 防御：线程组关系未初始化时，退化为本线程。
            return self.thread_cputime_ns();
        }

        let mut total = leader.thread_cputime_ns();
        let ti = leader.threads_read_irqsave();
        for t in &ti.group_tasks {
            if let Some(p) = t.upgrade() {
                total = total.saturating_add(p.thread_cputime_ns());
            }
        }
        total
    }

    #[inline(always)]
    pub fn account_utime(&self, ns: u64) {
        if ns == 0 {
            return;
        }
        self.cpu_time.utime.fetch_add(ns, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn account_stime(&self, ns: u64) {
        if ns == 0 {
            return;
        }
        self.cpu_time.stime.fetch_add(ns, Ordering::Relaxed);
    }

    #[inline(always)]
    pub fn add_sum_exec_runtime(&self, ns: u64) {
        self.cpu_time
            .sum_exec_runtime
            .fetch_add(ns, Ordering::Relaxed);
    }
}
