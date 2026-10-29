use alloc::{collections::LinkedList, format, string::String, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use system_error::SystemError;

use crate::libs::spinlock::SpinLock;

use super::*;

#[derive(Debug)]
struct FakeClock {
    data: SpinLock<ClocksourceData>,
    cycle: AtomicU64,
    reject_updates: AtomicBool,
}

impl FakeClock {
    fn new(name: &str, rating: i32, flags: ClocksourceFlags) -> Arc<Self> {
        let clock = Arc::new(Self {
            data: SpinLock::new(ClocksourceData {
                name: name.into(),
                rating,
                mask: ClocksourceMask::new(u64::MAX),
                mult: 1,
                shift: 0,
                max_cycles: u64::MAX,
                max_idle_ns: 0,
                flags,
                watchdog_last: CycleNum::new(0),
                cs_last: CycleNum::new(0),
                uncertainty_margin: 0,
                maxadj: 0,
            }),
            cycle: AtomicU64::new(0),
            reject_updates: AtomicBool::new(false),
        });
        clock
    }
}

impl Clocksource for FakeClock {
    fn read(&self) -> CycleNum {
        CycleNum::new(self.cycle.load(Ordering::Relaxed))
    }

    fn clocksource_data(&self) -> ClocksourceData {
        self.data.lock_irqsave().clone()
    }

    fn update_clocksource_data(&self, update: ClocksourceUpdate) -> Result<(), SystemError> {
        if self.reject_updates.load(Ordering::Relaxed) {
            return Err(SystemError::EIO);
        }
        update.apply(&mut self.data.lock_irqsave());
        Ok(())
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        let raw = self as *const FakeClock;
        // SAFETY: `FakeClock` is private to this selftest and can only be
        // constructed by `new()`, which immediately places it in an `Arc`.
        // A trait call also holds a live reference, so incrementing the Arc
        // strong count before rebuilding the handle is valid.
        unsafe {
            Arc::increment_strong_count(raw);
            Arc::from_raw(raw)
        }
    }
}

struct Report {
    body: String,
    passed: usize,
    failed: usize,
}

impl Report {
    fn case(&mut self, name: &str, passed: bool) {
        if passed {
            self.passed += 1;
            self.body.push_str(&format!("clocksource.{name}=ok\n"));
        } else {
            self.failed += 1;
            self.body.push_str(&format!("clocksource.{name}=fail\n"));
        }
    }
}

pub(crate) fn run_clocksource_selftests() -> (usize, usize, String) {
    let mut report = Report {
        body: String::new(),
        passed: 0,
        failed: 0,
    };

    let valid = ClocksourceData {
        name: "valid".into(),
        rating: 100,
        mask: ClocksourceMask::new(u64::MAX),
        mult: 1_000,
        shift: 10,
        max_cycles: 10_000,
        max_idle_ns: 0,
        flags: ClocksourceFlags::empty(),
        watchdog_last: CycleNum::new(0),
        cs_last: CycleNum::new(0),
        uncertainty_margin: 0,
        maxadj: 100,
    };
    let mut invalid_mask = valid.clone();
    invalid_mask.mask = ClocksourceMask::new(0b1011);
    let mut invalid_deferment = valid.clone();
    invalid_deferment.max_cycles = 0;
    report.case(
        "registration_validation",
        validate_clocksource_conversion(&valid, true).is_ok()
            && validate_clocksource_conversion(&invalid_mask, true) == Err(SystemError::EINVAL)
            && validate_clocksource_conversion(&invalid_deferment, true)
                == Err(SystemError::EINVAL),
    );

    let mut typed_metadata = valid.clone();
    let immutable_before = (
        typed_metadata.name.clone(),
        typed_metadata.mask,
        typed_metadata.mult,
        typed_metadata.shift,
        typed_metadata.max_cycles,
        typed_metadata.max_idle_ns,
    );
    ClocksourceUpdate::SetRating(321).apply(&mut typed_metadata);
    ClocksourceUpdate::MarkUnstable.apply(&mut typed_metadata);
    ClocksourceUpdate::ResetWatchdog.apply(&mut typed_metadata);
    report.case(
        "runtime_update_preserves_conversion",
        immutable_before
            == (
                typed_metadata.name.clone(),
                typed_metadata.mask,
                typed_metadata.mult,
                typed_metadata.shift,
                typed_metadata.max_cycles,
                typed_metadata.max_idle_ns,
            )
            && typed_metadata.rating == 321,
    );

    let first: Arc<dyn Clocksource> = FakeClock::new("same", 100, ClocksourceFlags::empty());
    let second: Arc<dyn Clocksource> = FakeClock::new("same", 100, ClocksourceFlags::empty());
    let mut identity_list = LinkedList::from([first.clone(), second.clone()]);
    let removed = remove_clocksource_identity(&mut identity_list, &first);
    report.case(
        "arc_identity_removal",
        removed
            && identity_list.len() == 1
            && identity_list
                .front()
                .is_some_and(|remaining| Arc::ptr_eq(remaining, &second)),
    );

    let target: Arc<dyn Clocksource> = FakeClock::new("target", 300, ClocksourceFlags::empty());
    let replacement: Arc<dyn Clocksource> =
        FakeClock::new("replacement", 200, ClocksourceFlags::empty());
    let must_verify: Arc<dyn Clocksource> = FakeClock::new(
        "must-verify",
        500,
        ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY,
    );
    let unstable: Arc<dyn Clocksource> =
        FakeClock::new("unstable", 600, ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
    let candidates = LinkedList::from([target.clone(), must_verify, unstable, replacement.clone()]);
    report.case(
        "watchdog_replacement",
        select_watchdog_replacement(&candidates, &target)
            .is_some_and(|selected| Arc::ptr_eq(&selected, &replacement)),
    );
    let no_replacement = LinkedList::from([target.clone()]);
    report.case(
        "watchdog_replacement_required",
        select_watchdog_replacement(&no_replacement, &target).is_none(),
    );

    let watched: Arc<dyn Clocksource> =
        FakeClock::new("watched", 400, ClocksourceFlags::CLOCK_SOURCE_MUST_VERIFY);
    let reference = Some(target.clone());
    report.case(
        "must_verify_starts_existing_reference",
        !watchdog_has_sources(&reference, &LinkedList::from([target.clone()]))
            && watchdog_has_sources(&reference, &LinkedList::from([target.clone(), watched])),
    );
    report.case(
        "watchdog_restart_deadline",
        watchdog_next_expiry(10_000) == 10_000 + WATCHDOG_INTERVAL
            && watchdog_next_expiry(u64::MAX) == u64::MAX,
    );

    let stable_a: Arc<dyn Clocksource> = FakeClock::new("stable-a", 100, ClocksourceFlags::empty());
    let unstable_a: Arc<dyn Clocksource> =
        FakeClock::new("unstable-a", 100, ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
    let unstable_b: Arc<dyn Clocksource> =
        FakeClock::new("unstable-b", 100, ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
    let stable_b: Arc<dyn Clocksource> = FakeClock::new("stable-b", 100, ClocksourceFlags::empty());
    let mut mixed = LinkedList::from([
        unstable_a.clone(),
        stable_a.clone(),
        unstable_b.clone(),
        stable_b.clone(),
    ]);
    let removed = remove_unstable_clocksources(&mut mixed);
    report.case(
        "multiple_unstable_cleanup",
        removed.len() == 2
            && Arc::ptr_eq(&removed[0], &unstable_a)
            && Arc::ptr_eq(&removed[1], &unstable_b)
            && mixed.len() == 2
            && mixed
                .front()
                .is_some_and(|source| Arc::ptr_eq(source, &stable_a))
            && mixed
                .back()
                .is_some_and(|source| Arc::ptr_eq(source, &stable_b)),
    );

    let cycles = u64::MAX;
    let mult = u32::MAX;
    let shift = 32;
    let expected = (((cycles as u128) * (mult as u128)) >> shift) as u64;
    report.case(
        "large_delta_u128_conversion",
        clocksource_cyc2ns(CycleNum::new(cycles), mult, shift) == expected,
    );

    (report.passed, report.failed, report.body)
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn heap_denied_watchdog() -> Result<bool, SystemError> {
    use crate::mm::oom::denial;
    let flags = ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS;
    let reference = FakeClock::new("heap-reference", 200, flags);
    let stable = FakeClock::new("heap-stable", 100, flags);
    let skewed = FakeClock::new("heap-skewed", 100, flags);
    let reference_trait: Arc<dyn Clocksource> = reference.clone();
    let list: LinkedList<Arc<dyn Clocksource>> =
        LinkedList::from([reference_trait.clone(), stable.clone(), skewed.clone()]);
    let mut state = ClocksourceWatchdog::new();
    state.watchdog = Some(reference_trait);
    let name = stable.clocksource_data().name;
    let passed = {
        let _scope = denial::Scope::enter(true);
        let mut ok = denial::probe_denied();
        let started_before = clock();
        state.clocksource_start_watchdog(true);
        ok &= state.is_running
            && state.timer_expires >= watchdog_next_expiry(started_before)
            && !state.is_due(started_before);
        ok &= !state.is_due(state.timer_expires - 1) && state.is_due(state.timer_expires);
        run_watchdog(&mut state, &list)?;
        reference.cycle.store(1_000_000, Ordering::Relaxed);
        stable.cycle.store(1_000_000, Ordering::Relaxed);
        skewed.cycle.store(1_000_000, Ordering::Relaxed);
        run_watchdog(&mut state, &list)?;
        let data = stable.clocksource_data();
        ok &= Arc::ptr_eq(&name, &data.name)
            && data
                .flags
                .contains(ClocksourceFlags::CLOCK_SOURCE_VALID_FOR_HRES)
            && !data.flags.contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
        reference.cycle.store(2_000_000, Ordering::Relaxed);
        stable.cycle.store(2_000_000, Ordering::Relaxed);
        skewed.cycle.store(
            2_000_000 + u64::from(WATCHDOG_THRESHOLD) + 1,
            Ordering::Relaxed,
        );
        run_watchdog(&mut state, &list)?;
        ok &= skewed
            .clocksource_data()
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
        // An excessively delayed reference must skip skew detection while
        // updating samples. Only private clocks are sampled by this fixture.
        reference
            .cycle
            .store(2_000_001 + WATCHDOG_INTERVAL_MAX_NS, Ordering::Relaxed);
        stable
            .cycle
            .store(2_000_001 + WATCHDOG_INTERVAL_MAX_NS, Ordering::Relaxed);
        run_watchdog(&mut state, &list)?;
        ok &= !stable
            .clocksource_data()
            .flags
            .contains(ClocksourceFlags::CLOCK_SOURCE_UNSTABLE);
        stable.reject_updates.store(true, Ordering::Relaxed);
        state.timer_expires = 0;
        let error_before = clock();
        ok &= run_watchdog(&mut state, &list) == Err(SystemError::EIO)
            && state.timer_expires >= watchdog_next_expiry(error_before);
        stable.reject_updates.store(false, Ordering::Relaxed);
        state.clocksource_stop_watchdog(false);
        ok &= !state.is_due(u64::MAX);
        let started_before = clock();
        state.clocksource_start_watchdog(true);
        ok &= state.is_running
            && state.timer_expires >= watchdog_next_expiry(started_before)
            && !state.is_due(started_before);
        ok && denial::probe_denied()
    };
    Ok(passed)
}
