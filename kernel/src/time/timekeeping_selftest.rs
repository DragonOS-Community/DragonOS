use alloc::{
    format,
    string::String,
    sync::{Arc, Weak},
};
use core::sync::atomic::{AtomicU64, Ordering};

use crate::{
    libs::spinlock::SpinLock,
    time::clocksource::{ClocksourceData, ClocksourceFlags, ClocksourceUpdate, CycleNum},
};

use super::*;

#[derive(Debug)]
struct FakeClock {
    inner: SpinLock<FakeClockInner>,
    cycle: AtomicU64,
    reads: AtomicU64,
}

#[derive(Debug)]
struct FakeClockInner {
    data: ClocksourceData,
    self_ref: Weak<FakeClock>,
}

impl FakeClock {
    fn new(name: &str, mask: u64, mult: u32, shift: u32, max_cycles: u64) -> Arc<Self> {
        let clock = Arc::new(Self {
            inner: SpinLock::new(FakeClockInner {
                data: ClocksourceData {
                    name: name.into(),
                    rating: 100,
                    mask: ClocksourceMask::new(mask),
                    mult,
                    shift,
                    max_cycles,
                    max_idle_ns: 0,
                    flags: ClocksourceFlags::CLOCK_SOURCE_IS_CONTINUOUS,
                    watchdog_last: CycleNum::new(0),
                    cs_last: CycleNum::new(0),
                    uncertainty_margin: 0,
                    maxadj: 0,
                },
                self_ref: Weak::new(),
            }),
            cycle: AtomicU64::new(0),
            reads: AtomicU64::new(0),
        });
        clock.inner.lock().self_ref = Arc::downgrade(&clock);
        clock
    }

    fn set_cycle(&self, cycle: u64) {
        self.cycle.store(cycle, Ordering::SeqCst);
    }

    fn reads(&self) -> u64 {
        self.reads.load(Ordering::SeqCst)
    }
}

impl Clocksource for FakeClock {
    fn read(&self) -> CycleNum {
        self.reads.fetch_add(1, Ordering::SeqCst);
        CycleNum::new(self.cycle.load(Ordering::SeqCst))
    }

    fn enable(&self) -> Result<i32, SystemError> {
        Ok(0)
    }

    fn clocksource_data(&self) -> ClocksourceData {
        self.inner.lock_irqsave().data.clone()
    }

    fn update_clocksource_data(&self, update: ClocksourceUpdate) -> Result<(), SystemError> {
        update.apply(&mut self.inner.lock_irqsave().data);
        Ok(())
    }

    fn clocksource(&self) -> Arc<dyn Clocksource> {
        match self.inner.lock_irqsave().self_ref.upgrade() {
            Some(clock) => clock,
            None => FakeClock::new("detached", u64::MAX, 1, 0, u64::MAX),
        }
    }
}

struct Report {
    text: String,
    passed: usize,
    failed: usize,
}

impl Report {
    fn new() -> Self {
        Self {
            text: String::new(),
            passed: 0,
            failed: 0,
        }
    }

    fn case(&mut self, name: &str, ok: bool) {
        if ok {
            self.passed += 1;
            self.text.push_str(&format!("timekeeping.{name}=ok\n"));
        } else {
            self.failed += 1;
            self.text.push_str(&format!("timekeeping.{name}=fail\n"));
        }
    }
}

// Only local clocks, timekeepers and synthetic counter samples below. These
// fixtures exercise the production pure helpers and conversion installation,
// not the global atomic commit, hardware notification or CPU sample count.
fn jiffies_handoff_continuity() -> bool {
    let old = FakeClock::new("jiffy-old", u64::MAX, 1, 1, u64::MAX / 2);
    old.set_cycle(100);
    let local = Timekeeper::new();
    if local
        .timekeeper_setup_internals(old.clone() as Arc<dyn Clocksource>)
        .is_err()
    {
        return false;
    }
    old.set_cycle(100 + 5 * JIFFY_NS + 1);
    let coarse = FakeClock::new(
        "jiffy-coarse",
        u64::MAX,
        (2 * JIFFY_NS) as u32,
        1,
        u64::MAX / 2,
    );
    coarse.inner.lock_irqsave().data.flags = ClocksourceFlags::empty();
    let coarse_dyn = coarse.clone() as Arc<dyn Clocksource>;
    let data = match validated_clocksource_data(&coarse_dyn) {
        Ok(data) => data,
        Err(_) => return false,
    };
    let mut tk = local.inner.write_irqsave();
    tk.next_jiffy_ns = Some(JIFFY_NS);
    tk.forward();
    let base = match tk.mono.as_ref() {
        Some(base) => base.base_ns,
        None => return false,
    };
    let flags = old.clocksource_data().flags;
    let (add, next) = match jiffy_handoff(JIFFY_NS, base, &flags) {
        Ok(result) => result,
        Err(_) => return false,
    };
    if (add, next) != (2, 3 * JIFFY_NS) {
        return false;
    }
    // The real install samples the jiffies counter only after committing add.
    // Here only the private coarse clock's counter changes.
    coarse.set_cycle(10 + add);
    tk.next_jiffy_ns = Some(next);
    tk.install_forwarded(coarse_dyn.clone(), &data);
    let fraction_preserved =
        tk.mono.as_ref().is_some_and(|b| {
            b.base_ns == 5 * JIFFY_NS / 2 && b.fraction == 1 && b.cycle_last == 12
        }) && tk.raw.as_ref().is_some_and(|b| {
            b.base_ns == 5 * JIFFY_NS / 2 && b.fraction == 1 && b.cycle_last == 12
        });
    let (add, next) = match jiffy_advance(next, tk.monotonic_ns(), false, true) {
        Ok(result) => result,
        Err(_) => return false,
    };
    if add != 1 {
        return false;
    }
    coarse.set_cycle(12 + add);
    tk.next_jiffy_ns = Some(next);
    tk.forward();
    let coarse_progress = tk.monotonic_ns() == 7 * JIFFY_NS / 2
        && tk.mono.as_ref().is_some_and(|b| b.fraction == 1)
        && next == 4 * JIFFY_NS;
    drop(tk);
    let resumed = FakeClock::new("jiffy-resumed", u64::MAX, 1, 1, u64::MAX / 2);
    resumed.set_cycle(900);
    if local
        .timekeeper_setup_internals(resumed.clone() as Arc<dyn Clocksource>)
        .is_err()
    {
        return false;
    }
    let tk = local.inner.read_irqsave();
    fraction_preserved
        && coarse_progress
        && tk.next_jiffy_ns == Some(4 * JIFFY_NS)
        && tk.monotonic_ns() == 7 * JIFFY_NS / 2
        && tk
            .mono
            .as_ref()
            .is_some_and(|b| b.fraction == 1 && b.cycle_last == 900)
        && tk.raw.as_ref().is_some_and(|b| {
            b.base_ns == 7 * JIFFY_NS / 2 && b.fraction == 1 && b.cycle_last == 900
        })
        && jiffy_advance(next, tk.monotonic_ns(), true, false) == Ok((0, next))
        && jiffy_advance(next, next, true, false) == Ok((1, 5 * JIFFY_NS))
}

fn jiffies_unstable_handoff() -> bool {
    let old = FakeClock::new("jiffy-failed", u64::MAX, 1, 0, u64::MAX / 2);
    old.set_cycle(100);
    let local = Timekeeper::new();
    if local
        .timekeeper_setup_internals(old.clone() as Arc<dyn Clocksource>)
        .is_err()
    {
        return false;
    }
    old.set_cycle(100 + 1000 * JIFFY_NS);
    if old
        .update_clocksource_data(ClocksourceUpdate::MarkUnstable)
        .is_err()
    {
        return false;
    }
    let new = FakeClock::new("jiffy-recovered", u64::MAX, 1, 0, u64::MAX / 2);
    new.set_cycle(900);
    let new_dyn = new.clone() as Arc<dyn Clocksource>;
    let data = match validated_clocksource_data(&new_dyn) {
        Ok(data) => data,
        Err(_) => return false,
    };
    let mut tk = local.inner.write_irqsave();
    tk.next_jiffy_ns = Some(3 * JIFFY_NS);
    tk.forward();
    let flags = old.clocksource_data().flags;
    let base = match tk.mono.as_ref() {
        Some(base) => base.base_ns,
        None => return false,
    };
    let (add, next) = match jiffy_handoff(3 * JIFFY_NS, base, &flags) {
        Ok(result) => result,
        Err(_) => return false,
    };
    if (add, next) != (0, 1001 * JIFFY_NS) {
        return false;
    }
    tk.next_jiffy_ns = Some(next);
    tk.install_forwarded(new_dyn, &data);
    new.set_cycle(900 + JIFFY_NS / 2);
    let early = jiffy_advance(next, tk.monotonic_ns(), true, false);
    new.set_cycle(900 + JIFFY_NS);
    add == 0
        && early == Ok((0, next))
        && jiffy_advance(next, tk.monotonic_ns(), true, false) == Ok((1, 1002 * JIFFY_NS))
}

pub(crate) fn run_timekeeping_selftests() -> (usize, usize, String) {
    let mut report = Report::new();

    report.case(
        "jiffies_phase_elapsed",
        jiffy_advance(3 * JIFFY_NS, 5 * JIFFY_NS / 2, true, true) == Ok((0, 3 * JIFFY_NS))
            && jiffy_advance(3 * JIFFY_NS, 3 * JIFFY_NS, true, false) == Ok((1, 4 * JIFFY_NS))
            && jiffy_advance(3 * JIFFY_NS, 13 * JIFFY_NS / 2, true, false) == Ok((4, 7 * JIFFY_NS))
            && jiffy_advance(7 * JIFFY_NS, 13 * JIFFY_NS / 2, true, true) == Ok((0, 7 * JIFFY_NS))
            && jiffy_advance(7 * JIFFY_NS, 7 * JIFFY_NS, true, false) == Ok((1, 8 * JIFFY_NS)),
    );
    report.case(
        "jiffies_fallback_gate",
        jiffy_advance(3 * JIFFY_NS, 1000 * JIFFY_NS, false, false) == Ok((0, 3 * JIFFY_NS))
            && jiffy_advance(3 * JIFFY_NS, 1000 * JIFFY_NS, false, true) == Ok((1, 4 * JIFFY_NS))
            && jiffy_handoff(3 * JIFFY_NS, 1000 * JIFFY_NS, &ClocksourceFlags::empty())
                == Ok((0, 3 * JIFFY_NS)),
    );
    report.case(
        "jiffies_phase_overflow",
        jiffy_advance(u64::MAX, u64::MAX - 1, true, false) == Ok((0, u64::MAX))
            && jiffy_advance(u64::MAX, u64::MAX, true, false) == Err(SystemError::EOVERFLOW)
            && jiffy_advance(u64::MAX, 0, false, true) == Err(SystemError::EOVERFLOW)
            && jiffy_handoff(0, u64::MAX, &ClocksourceFlags::CLOCK_SOURCE_UNSTABLE)
                == Err(SystemError::EOVERFLOW),
    );
    report.case("jiffies_source_handoff", jiffies_handoff_continuity());
    report.case("jiffies_unstable_handoff", jiffies_unstable_handoff());

    report.case(
        "contiguous_masks",
        mask_is_contiguous(0x00ff_ffff)
            && mask_is_contiguous(u32::MAX as u64)
            && mask_is_contiguous(u64::MAX)
            && !mask_is_contiguous(0)
            && !mask_is_contiguous(0x00ff_fffe),
    );

    report.case(
        "wrap_24",
        clocksource_delta(0x5, 0x00ff_fff0, 0x00ff_ffff, 0x00ff_ffff) == Ok(0x15),
    );
    report.case(
        "defer_boundary",
        clocksource_delta(8, 0, 0xff, 8) == Ok(8)
            && clocksource_delta(9, 0, 0xff, 8) == Ok(8)
            && clocksource_delta(1, 0, 0xfe, 8) == Err(SystemError::EINVAL),
    );

    #[cfg(target_arch = "x86_64")]
    {
        report.case(
            "tsc_deadline_conversion",
            crate::arch::driver::apic::apic_timer::tsc_deadline_conversion_selftest(),
        );
        // Linux x86 validates the mask's high bit before conversion capping.
        // Keep genuine forward overflow below half a counter period.
        let delta_boundaries = [0xff, 0x00ff_ffff, u64::MAX].into_iter().all(|mask| {
            let half = (mask >> 1) + 1;
            clocksource_delta(55, 55, mask, 8) == Ok(0)
                && clocksource_delta(99, 100, mask, 8) == Ok(0)
                && clocksource_delta(5, mask - 15, mask, mask) == Ok(21)
                && clocksource_delta(half - 1, 0, mask, mask) == Ok(half - 1)
                && clocksource_delta(half, 0, mask, mask) == Ok(0)
                && clocksource_delta(half + 1, 0, mask, mask) == Ok(0)
                && clocksource_delta(8, 0, mask, 8) == Ok(8)
                && clocksource_delta(9, 0, mask, 8) == Ok(8)
        }) && clocksource_delta(1, 0, 1, 1) == Ok(0)
            && clocksource_delta(0, 0, 0, 1) == Err(SystemError::EINVAL)
            && clocksource_delta(0, 1, 0xfe, 8) == Err(SystemError::EINVAL)
            && clocksource_delta(0, 1, 0xff, 0) == Err(SystemError::EINVAL)
            && clocksource_delta(0, 1, 0xff, 0x100) == Err(SystemError::EINVAL);
        report.case("backward_delta_boundaries", delta_boundaries);

        // A private timekeeper only: the real clocksource and hardware are
        // never changed. Old modulo-then-cap turns 100 -> 99 into 12ns.
        let read_clock = FakeClock::new("backward-read", 0xff, 3, 1, 8);
        read_clock.set_cycle(100);
        let read_keeper = Timekeeper::new();
        let read_setup = read_keeper
            .timekeeper_setup_internals(read_clock.clone() as Arc<dyn Clocksource>)
            .is_ok();
        read_clock.set_cycle(99);
        let state = read_keeper.inner.read_irqsave();
        let read_ok = match (&state.mono, &state.raw) {
            (Some(mono), Some(raw)) => {
                state.monotonic_ns() == 0
                    && state.raw_ns() == 0
                    && state.realtime_ns() == 0
                    && state.boottime_ns() == 0
                    && mono.base_ns == 0
                    && raw.base_ns == 0
                    && mono.fraction == 0
                    && raw.fraction == 0
                    && mono.cycle_last == 100
                    && raw.cycle_last == 100
            }
            _ => false,
        };
        drop(state);
        report.case("backward_read_zero_elapsed", read_setup && read_ok);

        let forward_clock = FakeClock::new("backward-forward", 0xff, 3, 1, 8);
        forward_clock.set_cycle(100);
        let forward_keeper = Timekeeper::new();
        let forward_setup = forward_keeper
            .timekeeper_setup_internals(forward_clock.clone() as Arc<dyn Clocksource>)
            .is_ok();
        let forward_ok = if forward_setup {
            let mut state = forward_keeper.inner.write_irqsave();
            forward_clock.set_cycle(101);
            let reads = forward_clock.reads();
            state.forward();
            let first = state.mono.as_ref().is_some_and(|base| {
                base.base_ns == 1 && base.fraction == 1 && base.cycle_last == 101
            }) && state.raw.as_ref().is_some_and(|base| {
                base.base_ns == 1 && base.fraction == 1 && base.cycle_last == 101
            }) && forward_clock.reads() == reads + 1;

            // A writer still reanchors both bases to the current sample,
            // retaining its fractional carry when the validated delta is 0.
            forward_clock.set_cycle(100);
            let reads = forward_clock.reads();
            state.forward();
            let backward = state.mono.as_ref().is_some_and(|base| {
                base.base_ns == 1 && base.fraction == 1 && base.cycle_last == 100
            }) && state.raw.as_ref().is_some_and(|base| {
                base.base_ns == 1 && base.fraction == 1 && base.cycle_last == 100
            }) && forward_clock.reads() == reads + 1;

            forward_clock.set_cycle(101);
            let reads = forward_clock.reads();
            state.forward();
            let progress = state.mono.as_ref().is_some_and(|base| {
                base.base_ns == 3 && base.fraction == 0 && base.cycle_last == 101
            }) && state.raw.as_ref().is_some_and(|base| {
                base.base_ns == 3 && base.fraction == 0 && base.cycle_last == 101
            }) && forward_clock.reads() == reads + 1;
            first && backward && progress
        } else {
            false
        };
        report.case(
            "backward_forward_fraction_progress",
            forward_setup && forward_ok,
        );
    }

    report.case(
        "fraction_carry",
        scale_delta(1, 3, 1, 1) == Ok((2, 0)) && scale_delta(7, 1, 3, 7) == Ok((1, 6)),
    );
    report.case(
        "shift_boundaries",
        scale_delta(3, 2, 0, 0) == Ok((6, 0))
            && scale_delta(0, 1, 63, u64::MAX >> 1) == Ok((0, u64::MAX >> 1))
            && scale_delta(1, 1, 63, u64::MAX >> 1) == Ok((1, 0))
            && scale_delta(1, 1, 64, 0) == Err(SystemError::EINVAL)
            && scale_delta(1, 1, 0, 1) == Err(SystemError::EINVAL),
    );
    report.case(
        "fraction_rescale",
        rescale_fraction(3, 2, 4) == 12 && rescale_fraction(12, 4, 2) == 3,
    );
    report.case(
        "base_overflow",
        add_elapsed(u64::MAX - 1, 1) == Ok(u64::MAX)
            && add_elapsed(u64::MAX, 1) == Err(SystemError::EOVERFLOW),
    );
    report.case(
        "timespec_normalization",
        ns_to_timespec(-1)
            == (PosixTimeSpec {
                tv_sec: -1,
                tv_nsec: 999_999_999,
            })
            && ns_to_timespec(1_000_000_001)
                == (PosixTimeSpec {
                    tv_sec: 1,
                    tv_nsec: 1,
                }),
    );
    report.case(
        "timeout_extreme_saturation",
        PosixTimeSpec {
            tv_sec: i64::MAX,
            tv_nsec: 0,
        }
        .to_ktime_ns()
            == i64::MAX as u64
            && PosixTimeSpec {
                tv_sec: i64::MAX,
                tv_nsec: 0,
            }
            .saturating_add_ktime(&PosixTimeSpec {
                tv_sec: 1,
                tv_nsec: 0,
            }) == PosixTimeSpec::KTIME_MAX,
    );
    report.case(
        "timeout_add_carry",
        PosixTimeSpec {
            tv_sec: 1,
            tv_nsec: 900_000_000,
        }
        .saturating_add_ktime(&PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: 200_000_000,
        }) == (PosixTimeSpec {
            tv_sec: 2,
            tv_nsec: 100_000_000,
        }),
    );
    report.case(
        "timeout_reverse_sub_zero",
        PosixTimeSpec {
            tv_sec: 1,
            tv_nsec: 0,
        }
        .saturating_sub_timespec(&PosixTimeSpec {
            tv_sec: 2,
            tv_nsec: 0,
        }) == PosixTimeSpec::default(),
    );
    report.case(
        "timeout_validation",
        PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: 999_999_999,
        }
        .is_valid_timeout()
            && !PosixTimeSpec {
                tv_sec: -1,
                tv_nsec: 0,
            }
            .is_valid_timeout()
            && !PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: -1,
            }
            .is_valid_timeout()
            && !PosixTimeSpec {
                tv_sec: 0,
                tv_nsec: NSEC_PER_SEC as i64,
            }
            .is_valid_timeout(),
    );

    let source_a = FakeClock::new("fake-a", u64::MAX, 2, 1, u64::MAX / 2);
    source_a.set_cycle(100);
    let source_a_dyn = source_a.clone() as Arc<dyn Clocksource>;
    let local = Timekeeper::new();
    let setup_a_ok = local
        .timekeeper_setup_internals(source_a_dyn.clone())
        .is_ok();
    source_a.set_cycle(150);

    let source_b = FakeClock::new("fake-b", u64::MAX, 1, 0, u64::MAX);
    source_b.set_cycle(900);
    let source_b_dyn = source_b.clone() as Arc<dyn Clocksource>;
    let old_reads_before = source_a.reads();
    let new_reads_before = source_b.reads();
    let switch_ok = local
        .timekeeper_setup_internals(source_b_dyn.clone())
        .is_ok();
    let switched = local.inner.read_irqsave();
    let continuity_ok = switched.mono.as_ref().is_some_and(|base| {
        Arc::ptr_eq(&base.clock, &source_b_dyn) && base.base_ns == 50 && base.cycle_last == 900
    }) && switched.raw.as_ref().is_some_and(|base| {
        Arc::ptr_eq(&base.clock, &source_b_dyn) && base.base_ns == 50 && base.cycle_last == 900
    }) && switched.clocksource_generation == 2;
    drop(switched);
    report.case(
        "switch_continuity",
        setup_a_ok
            && switch_ok
            && continuity_ok
            && source_a.reads() == old_reads_before + 1
            && source_b.reads() == new_reads_before + 1,
    );

    let generation_before_noop = local.inner.read_irqsave().clocksource_generation;
    let reads_before_noop = source_b.reads();
    let noop_ok = local
        .timekeeper_setup_internals(source_b_dyn.clone())
        .is_ok();
    report.case(
        "same_source_noop",
        noop_ok
            && local.inner.read_irqsave().clocksource_generation == generation_before_noop
            && source_b.reads() == reads_before_noop,
    );

    let invalid = FakeClock::new("invalid", u64::MAX, 0, 0, u64::MAX);
    let generation_before_invalid = local.inner.read_irqsave().clocksource_generation;
    let invalid_ok = local
        .timekeeper_setup_internals(invalid.clone() as Arc<dyn Clocksource>)
        .is_err();
    report.case(
        "switch_rollback_invalid_target",
        invalid_ok
            && invalid.reads() == 0
            && local.inner.read_irqsave().clocksource_generation == generation_before_invalid
            && local
                .current_clocksource()
                .is_some_and(|clock| Arc::ptr_eq(&clock, &source_b_dyn)),
    );

    let invalid_configs = [
        FakeClock::new("mask-zero", 0, 1, 0, 1),
        FakeClock::new("mask-gap", 0xfe, 1, 0, 1),
        FakeClock::new("shift-64", u64::MAX, 1, 64, 1),
        FakeClock::new("max-zero", u64::MAX, 1, 0, 0),
        FakeClock::new("max-over-mask", 0xff, 1, 0, 0x100),
    ];
    report.case(
        "source_validation",
        invalid_configs.into_iter().all(|clock| {
            local
                .timekeeper_setup_internals(clock.clone() as Arc<dyn Clocksource>)
                .is_err()
                && clock.reads() == 0
        }),
    );

    let wall_clock = FakeClock::new("wall", u64::MAX, 1, 0, u64::MAX);
    wall_clock.set_cycle(100);
    let wall_keeper = Timekeeper::new();
    let wall_setup_ok = wall_keeper
        .timekeeper_setup_internals(wall_clock.clone() as Arc<dyn Clocksource>)
        .is_ok();
    wall_clock.set_cycle(150);
    let requested = validate_settimeofday(PosixTimeSpec {
        tv_sec: 1,
        tv_nsec: 0,
    });
    let wall_set_ok = requested
        .and_then(|requested| {
            settimeofday_locked(&mut wall_keeper.inner.write_irqsave(), requested)
        })
        .is_ok();
    let wall_state = wall_keeper.inner.read_irqsave();
    let offset_after_success = wall_state.realtime_offset_ns;
    let wall_domains_ok = wall_state
        .mono
        .as_ref()
        .is_some_and(|base| base.base_ns == 50)
        && wall_state
            .raw
            .as_ref()
            .is_some_and(|base| base.base_ns == 50)
        && wall_state.boottime_offset_ns == 0
        && wall_state.realtime_ns() == NSEC_PER_SEC as i128;
    drop(wall_state);
    report.case(
        "settimeofday_domains",
        wall_setup_ok && wall_set_ok && wall_domains_ok,
    );

    wall_clock.set_cycle(160);
    let generation_before_early = wall_keeper.inner.read_irqsave().clocksource_generation;
    let early_result = settimeofday_locked(&mut wall_keeper.inner.write_irqsave(), 0);
    let early_state = wall_keeper.inner.read_irqsave();
    let early_ok = early_result == Err(SystemError::EINVAL)
        && early_state
            .mono
            .as_ref()
            .is_some_and(|base| base.base_ns == 60)
        && early_state
            .raw
            .as_ref()
            .is_some_and(|base| base.base_ns == 60)
        && early_state.realtime_offset_ns == offset_after_success
        && early_state.clocksource_generation == generation_before_early;
    drop(early_state);
    report.case("settimeofday_early_forward_only", early_ok);

    let suspended_clock = FakeClock::new("suspended-wall", u64::MAX, 1, 0, u64::MAX);
    suspended_clock.set_cycle(100);
    let suspended_keeper = Timekeeper::new();
    let suspended_setup_ok = suspended_keeper
        .timekeeper_setup_internals(suspended_clock.clone() as Arc<dyn Clocksource>)
        .is_ok();
    suspended_clock.set_cycle(150);
    suspended_keeper.inner.write_irqsave().boottime_offset_ns = 100;
    let realtime_before_boottime_ok = validate_settimeofday(PosixTimeSpec {
        tv_sec: 0,
        tv_nsec: 120,
    })
    .and_then(|requested| {
        settimeofday_locked(&mut suspended_keeper.inner.write_irqsave(), requested)
    })
    .is_ok();
    let suspended_state = suspended_keeper.inner.read_irqsave();
    report.case(
        "settimeofday_allows_realtime_before_boottime",
        suspended_setup_ok
            && realtime_before_boottime_ok
            && suspended_state.realtime_ns() == 120
            && suspended_state.monotonic_ns() == 50
            && suspended_state.boottime_ns() == 150,
    );
    drop(suspended_state);

    let reads_before_bad_format = wall_clock.reads();
    let bad_format_ok = validate_settimeofday(PosixTimeSpec {
        tv_sec: -1,
        tv_nsec: 0,
    }) == Err(SystemError::EINVAL)
        && validate_settimeofday(PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: -1,
        }) == Err(SystemError::EINVAL)
        && validate_settimeofday(PosixTimeSpec {
            tv_sec: 0,
            tv_nsec: NSEC_PER_SEC as i64,
        }) == Err(SystemError::EINVAL)
        && validate_settimeofday(PosixTimeSpec {
            tv_sec: TIME_SETTOD_SEC_MAX - 1,
            tv_nsec: NSEC_PER_SEC as i64 - 1,
        })
        .is_ok()
        && validate_settimeofday(PosixTimeSpec {
            tv_sec: TIME_SETTOD_SEC_MAX,
            tv_nsec: 0,
        }) == Err(SystemError::EINVAL)
        && wall_clock.reads() == reads_before_bad_format;
    report.case("settimeofday_format_boundaries", bad_format_ok);

    let defer_source = FakeClock::new("defer", 0x00ff_ffff, 10, 2, 1);
    let defer_update_ok = defer_source
        .update_clocksource_data(ClocksourceUpdate::SetMaxAdjustment(1))
        .is_ok();
    let defer_dyn = defer_source as Arc<dyn Clocksource>;
    let (max_cycles, max_idle_ns) = defer_dyn.clocksource_max_deferment();
    let expected_idle = (((0x00ff_ffffu128 * 9) >> 2) >> 1) as u64;
    report.case(
        "max_deferment",
        defer_update_ok && max_cycles == 0x00ff_ffff && max_idle_ns == expected_idle,
    );

    (report.passed, report.failed, report.text)
}
