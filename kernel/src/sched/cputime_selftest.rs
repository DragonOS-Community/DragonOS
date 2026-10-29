//! Isolated checks for the two independent consumers of cumulative steal time.
//! No real shared page, runqueue, task runtime or CPU statistics are modified.

use alloc::{format, string::String};

use super::*;

fn rq_bounded() -> bool {
    let mut previous = 0;
    rq_steal_time(500, &mut previous, 100) == 100
        && previous == 500
        && rq_steal_time(500, &mut previous, 100) == 0
        && rq_steal_time(540, &mut previous, 100) == 40
}

fn rq_reset() -> bool {
    let mut previous = 500;
    rq_steal_time(10, &mut previous, 100) == 0
        && previous == 10
        && rq_steal_time(15, &mut previous, 100) == 5
}

fn usage_balance() -> bool {
    let stat = KernelCpuStat::new();
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    stat.account_steal(100, 20) == 20
        && stat.account_steal(100, 30) == 30
        && stat.account_steal(100, 100) == 50
        && stat.account_steal(100, 100) == 0
        && stat.get(CpuUsageStat::Steal) == 100
}

fn usage_reset_with_balance() -> bool {
    let stat = KernelCpuStat::new();
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    // The new total is still above consumed=10, but below observed=100.
    stat.account_steal(100, 10) == 10
        && stat.account_steal(50, 100) == 0
        && stat.account_steal(55, 100) == 5
        && stat.get(CpuUsageStat::Steal) == 15
}

fn zero_limit_observes_reset() -> bool {
    let stat = KernelCpuStat::new();
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    stat.account_steal(100, 0) == 0
        && stat.account_steal(50, 0) == 0
        && stat.account_steal(55, 100) == 5
        && stat.get(CpuUsageStat::Steal) == 5
}

fn independent_consumers() -> bool {
    let stat = KernelCpuStat::new();
    let mut rq_previous = 0;
    let _irq = unsafe { CurrentIrqArch::save_and_disable_irq() };
    rq_steal_time(100, &mut rq_previous, 10) == 10
        && stat.account_steal(100, 20) == 20
        && rq_steal_time(100, &mut rq_previous, 100) == 0
        && stat.account_steal(100, 100) == 80
        && stat.get(CpuUsageStat::Steal) == 100
}

pub(crate) fn run_virtual_time_selftests() -> (usize, usize, String) {
    let cases = [
        (
            "sched_clock",
            crate::sched::clock::run_sched_clock_selftests().is_ok(),
        ),
        ("rq_bounded", rq_bounded()),
        ("rq_reset", rq_reset()),
        ("usage_balance", usage_balance()),
        ("usage_reset_with_balance", usage_reset_with_balance()),
        ("zero_limit_observes_reset", zero_limit_observes_reset()),
        ("independent_consumers", independent_consumers()),
    ];
    let mut passed = 0;
    let mut failed = 0;
    let mut text = String::new();
    for (name, ok) in cases {
        passed += usize::from(ok);
        failed += usize::from(!ok);
        text.push_str(&format!(
            "virtual_time.{name}={}\n",
            if ok { "ok" } else { "fail" }
        ));
    }
    #[cfg(target_arch = "x86_64")]
    {
        let ok = crate::arch::kvm_para::steal_time::run_selftests().is_ok();
        passed += usize::from(ok);
        failed += usize::from(!ok);
        text.push_str(&format!(
            "virtual_time.kvm_steal_reader={}\n",
            if ok { "ok" } else { "fail" }
        ));
        let ok = crate::arch::reboot::run_kvm_shutdown_selftests().is_ok();
        passed += usize::from(ok);
        failed += usize::from(!ok);
        text.push_str(&format!(
            "virtual_time.kvm_shutdown_states={}\n",
            if ok { "ok" } else { "fail" }
        ));
    }
    (passed, failed, text)
}
