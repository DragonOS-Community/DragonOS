use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use alloc::sync::Arc;

use log::warn;

use super::{ProcessControlBlock, ProcessManager};

#[derive(Debug, Default)]
pub struct ProcessCpuTime {
    pub utime: AtomicU64,
    pub stime: AtomicU64,
    pub sum_exec_runtime: AtomicU64,
}

/// Settled values: safe to aggregate while holding thread membership locks.
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct CpuTimeSnapshot {
    pub user: u64,
    pub system: u64,
}

impl CpuTimeSnapshot {
    fn add(&mut self, other: Self) {
        self.user = self.user.saturating_add(other.user);
        self.system = self.system.saturating_add(other.system);
    }
}

impl ProcessControlBlock {
    #[inline(always)]
    pub fn cputime(&self) -> Arc<ProcessCpuTime> {
        self.cpu_time.clone()
    }

    /// Linux CPUCLOCK_SCHED: settle actual runtime, never project wall time.
    #[inline]
    pub fn thread_cputime_ns(&self) -> u64 {
        crate::sched::cputime::task_sched_runtime(self)
    }

    pub(super) fn settled_cputime(&self) -> CpuTimeSnapshot {
        CpuTimeSnapshot {
            user: self.cpu_time.utime.load(Ordering::Relaxed),
            system: self.cpu_time.stime.load(Ordering::Relaxed),
        }
    }

    /// Keep the existing dynamic PROF-like clock separate from SCHED time.
    pub(crate) fn thread_tick_cputime_ns(&self) -> u64 {
        let sample = self.settled_cputime();
        sample.user.saturating_add(sample.system)
    }

    /// 当前进程（线程组）的 CPU 时间（ns），语义对齐 Linux 的 CLOCK_PROCESS_CPUTIME_ID。
    ///
    /// SCHED runtime belongs to the shared signal state, including exited tasks.
    pub fn process_cputime_ns(&self) -> u64 {
        // Linux only settles the calling member; remote members contribute
        // their latest scheduler snapshots. Never acquire rq under membership.
        let current = ProcessManager::current_pcb();
        if Arc::ptr_eq(&current.process_signal(), &self.process_signal()) {
            current.thread_cputime_ns();
        }
        self.process_signal().cpu_runtime()
    }

    pub(crate) fn process_tick_cputime_ns(&self) -> u64 {
        let sample = self.process_cputime_snapshot();
        sample.user.saturating_add(sample.system)
    }

    fn process_cputime_snapshot(&self) -> CpuTimeSnapshot {
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
            return self.settled_cputime();
        }

        let ti = leader.threads_read_irqsave();
        // Exit accounts a thread under this same membership lock before
        // removing it from group_tasks.  A reader therefore observes the
        // thread either here or in the exited total, never in both/neither.
        let mut total = *leader.exited_thread_group_cputime_ns.lock();
        total.add(leader.settled_cputime());
        for t in &ti.group_tasks {
            if let Some(p) = t.upgrade() {
                total.add(p.settled_cputime());
            }
        }
        total
    }

    pub(super) fn add_exited_thread_group_cputime(&self, sample: CpuTimeSnapshot) {
        let mut total = self.exited_thread_group_cputime_ns.lock();
        total.add(sample);
    }

    /// Preserve signal_struct-like CPU history when non-leader exec promotes
    /// this task to thread-group leader.  The group-exec handoff has already
    /// quiesced every other member, so queries still select `old_leader` before
    /// the identity swap and select `self` afterwards.
    pub(crate) fn inherit_exited_thread_group_cputime_from(
        &self,
        old_leader: &ProcessControlBlock,
    ) {
        let inherited = *old_leader.exited_thread_group_cputime_ns.lock();
        self.add_exited_thread_group_cputime(inherited);
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
        if ns == 0 {
            return;
        }
        self.cpu_time
            .sum_exec_runtime
            .fetch_add(ns, Ordering::Relaxed);
        self.process_signal().account_cpu_runtime(ns);
    }
}
