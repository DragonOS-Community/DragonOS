//! Basic CPU accounting belongs to the cgroup node, not its optional CPU css.
//!
//! Task runtime and tick-classified user/system time are independent inputs.
//! Like Linux's cgroup base statistics, the latter are adjusted to the former
//! only when read. Keeping a stable parent chain preserves ancestor totals
//! after controller removal, task migration, or deletion of an empty child.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    libs::spinlock::SpinLock,
    sched::cputime::{kcpustat_cpu, CpuUsageStat},
    smp::cpu::smp_cpu_manager,
};

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct CpuTimeSnapshot {
    pub usage_ns: u64,
    pub user_ns: u64,
    pub system_ns: u64,
}

#[derive(Debug, Default)]
struct AdjustedCpuTime {
    user_ns: u64,
    system_ns: u64,
}

#[derive(Debug)]
pub(crate) struct CpuAccounting {
    parent: Option<Arc<CpuAccounting>>,
    runtime_ns: AtomicU64,
    user_ns: AtomicU64,
    system_ns: AtomicU64,
    adjusted: SpinLock<AdjustedCpuTime>,
}

impl CpuAccounting {
    pub(crate) fn new(parent: Option<Arc<Self>>) -> Arc<Self> {
        Arc::new(Self {
            parent,
            runtime_ns: AtomicU64::new(0),
            user_ns: AtomicU64::new(0),
            system_ns: AtomicU64::new(0),
            adjusted: SpinLock::new(AdjustedCpuTime::default()),
        })
    }

    pub(crate) fn account_runtime(&self, delta_ns: u64) {
        self.for_each_nonroot(|node| {
            node.runtime_ns.fetch_add(delta_ns, Ordering::Relaxed);
        });
    }

    pub(crate) fn account_cputime(&self, user: bool, delta_ns: u64) {
        self.for_each_nonroot(|node| {
            let field = if user { &node.user_ns } else { &node.system_ns };
            field.fetch_add(delta_ns, Ordering::Relaxed);
        });
    }

    fn for_each_nonroot(&self, mut account: impl FnMut(&Self)) {
        let mut node = self;
        while let Some(parent) = &node.parent {
            account(node);
            node = parent;
        }
    }

    pub(crate) fn snapshot(&self) -> CpuTimeSnapshot {
        if self.parent.is_none() {
            return Self::root_snapshot();
        }

        // Serialize readers before taking the atomic snapshot: otherwise an
        // older reader could publish smaller adjusted values after a newer one.
        let mut previous = self.adjusted.lock_irqsave();
        let runtime = self.runtime_ns.load(Ordering::Relaxed);
        let user = self.user_ns.load(Ordering::Relaxed);
        let system = self.system_ns.load(Ordering::Relaxed);
        if previous.user_ns.saturating_add(previous.system_ns) < runtime {
            let system = if system == 0 {
                0
            } else if user == 0 {
                runtime
            } else {
                ((system as u128 * runtime as u128) / (system as u128 + user as u128)) as u64
            };
            let mut system = system.max(previous.system_ns);
            let mut user = runtime - system;
            if user < previous.user_ns {
                user = previous.user_ns;
                system = runtime - user;
            }
            previous.user_ns = user;
            previous.system_ns = system;
        }
        CpuTimeSnapshot {
            usage_ns: runtime,
            user_ns: previous.user_ns,
            system_ns: previous.system_ns,
        }
    }

    fn root_snapshot() -> CpuTimeSnapshot {
        let mut snapshot = CpuTimeSnapshot::default();
        // Root includes IRQ/softirq time but excludes idle/iowait, matching
        // Linux root_cgroup_cputime rather than summing task execution time.
        for cpu in smp_cpu_manager().possible_cpus().iter_cpu() {
            let stat = kcpustat_cpu(cpu).snapshot();
            snapshot.user_ns = snapshot.user_ns.saturating_add(
                stat[CpuUsageStat::User as usize].saturating_add(stat[CpuUsageStat::Nice as usize]),
            );
            snapshot.system_ns = snapshot.system_ns.saturating_add(
                stat[CpuUsageStat::System as usize]
                    .saturating_add(stat[CpuUsageStat::Irq as usize])
                    .saturating_add(stat[CpuUsageStat::Softirq as usize]),
            );
        }
        snapshot.usage_ns = snapshot.user_ns.saturating_add(snapshot.system_ns);
        snapshot
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hierarchy_keeps_history_and_does_not_charge_root_twice() {
        let root = CpuAccounting::new(None);
        let parent = CpuAccounting::new(Some(root.clone()));
        let leaf = CpuAccounting::new(Some(parent.clone()));
        leaf.account_runtime(100);
        leaf.account_cputime(true, 80);
        leaf.account_cputime(false, 20);
        assert_eq!(parent.runtime_ns.load(Ordering::Relaxed), 100);
        assert_eq!(parent.user_ns.load(Ordering::Relaxed), 80);
        assert_eq!(root.runtime_ns.load(Ordering::Relaxed), 0);
        drop(leaf);
        assert_eq!(parent.runtime_ns.load(Ordering::Relaxed), 100);
    }

    #[test]
    fn adjustment_preserves_both_components_when_tick_ratio_changes() {
        let root = CpuAccounting::new(None);
        let node = CpuAccounting::new(Some(root));
        node.account_runtime(100);
        let first = node.snapshot();
        assert_eq!((first.user_ns, first.system_ns), (100, 0));
        node.account_cputime(false, 200);
        node.account_runtime(20);
        let second = node.snapshot();
        assert_eq!((second.user_ns, second.system_ns), (100, 20));
        node.account_cputime(true, 400);
        node.account_runtime(30);
        let third = node.snapshot();
        assert_eq!((third.user_ns, third.system_ns), (100, 50));
        assert_eq!(third.usage_ns, third.user_ns + third.system_ns);
        let repeated = node.snapshot();
        assert_eq!((repeated.user_ns, repeated.system_ns), (100, 50));
    }

    #[test]
    fn classification_scaling_does_not_overflow_u64() {
        let root = CpuAccounting::new(None);
        let node = CpuAccounting::new(Some(root));
        node.account_runtime(u64::MAX - 1);
        node.account_cputime(true, u64::MAX);
        node.account_cputime(false, u64::MAX);
        let stat = node.snapshot();
        assert_eq!(stat.user_ns, (u64::MAX - 1) / 2);
        assert_eq!(stat.system_ns, (u64::MAX - 1) / 2);
    }
}
