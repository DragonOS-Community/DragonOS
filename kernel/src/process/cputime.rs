use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::sync::Arc;

use log::warn;

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
        // 这里使用 Ordering::Relaxed：
        // - 只需要读取两个独立计数器的“某个一致快照”（不要求与其它内存状态建立 happens-before）。
        // - 对单个 AtomicU64 保证按地址一致性（coherence），满足 CPU-time 统计的近似/观测语义。
        // 如未来需要与其它状态强一致（例如结合序列号/结构体快照），再引入更强的同步原语。
        let ct = self.cputime();
        ct.utime.load(Ordering::Relaxed) + ct.stime.load(Ordering::Relaxed)
    }

    /// 当前进程（线程组）的 CPU 时间（ns），语义对齐 Linux 的 CLOCK_PROCESS_CPUTIME_ID。
    ///
    /// 说明：目前通过遍历线程组成员并累加每线程的 user+system 得到。
    pub fn process_cputime_ns(&self) -> u64 {
        static BAD_TGROUP_LOGGED: AtomicBool = AtomicBool::new(false);

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
            if BAD_TGROUP_LOGGED
                .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                warn!(
                    "process_cputime_ns fallback: invalid thread-group relation (pid={:?} tgid={:?} leader_pid={:?} leader_tgid={:?})",
                    self.raw_pid(),
                    self.tgid,
                    leader.raw_pid(),
                    leader.tgid,
                );
            }
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
