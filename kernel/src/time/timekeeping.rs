use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use log::{info, warn};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    exception::InterruptArch,
    libs::rwlock::RwLock,
    time::{
        jiffies::{clocksource_default_clock, jiffies_init},
        timekeep::ktime_get_real_ns,
        PosixTimeSpec,
    },
};

use super::{
    clocksource::{Clocksource, ClocksourceMask},
    syscall::PosixTimeval,
    NSEC_PER_SEC,
};

pub static TIMEKEEPING_SUSPENDED: AtomicBool = AtomicBool::new(false);

static mut __TIMEKEEPER: Option<Timekeeper> = None;

const TIME_SETTOD_SEC_MAX: i64 = i64::MAX / NSEC_PER_SEC as i64 - 30 * 365 * 24 * 60 * 60;

#[derive(Debug, Clone)]
struct TimekeeperReadBase {
    clock: Arc<dyn Clocksource>,
    mask: ClocksourceMask,
    cycle_last: u64,
    mult: u32,
    shift: u32,
    max_cycles: u64,
    base_ns: u64,
    fraction: u64,
}

impl TimekeeperReadBase {
    fn new(
        clock: Arc<dyn Clocksource>,
        data: &super::clocksource::ClocksourceData,
        cycle_last: u64,
        base_ns: u64,
    ) -> Option<Self> {
        if data.mask.bits() == 0
            || data.mult == 0
            || data.shift >= u64::BITS
            || data.max_cycles == 0
            || data.max_cycles > data.mask.bits()
            || !mask_is_contiguous(data.mask.bits())
        {
            return None;
        }

        Some(Self {
            clock,
            mask: data.mask,
            cycle_last,
            mult: data.mult,
            shift: data.shift,
            max_cycles: data.max_cycles,
            base_ns,
            fraction: 0,
        })
    }

    #[inline]
    fn delta(&self, cycle_now: u64) -> u64 {
        clocksource_delta(
            cycle_now,
            self.cycle_last,
            self.mask.bits(),
            self.max_cycles,
        )
        .expect("installed timekeeper has invalid clocksource conversion bounds")
    }

    #[inline]
    fn read_ns(&self, cycle_now: u64) -> u64 {
        let (elapsed, _) = scale_delta(self.delta(cycle_now), self.mult, self.shift, self.fraction)
            .expect("validated timekeeper conversion became invalid");
        add_elapsed(self.base_ns, elapsed).expect("timekeeper read overflow")
    }

    fn forward(&mut self, cycle_now: u64) {
        let (elapsed, fraction) =
            scale_delta(self.delta(cycle_now), self.mult, self.shift, self.fraction)
                .expect("validated timekeeper conversion became invalid");
        self.base_ns = add_elapsed(self.base_ns, elapsed).expect("timekeeper forward overflow");
        self.fraction = fraction;
        self.cycle_last = cycle_now;
    }
}

#[derive(Debug)]
pub struct TimekeeperData {
    mono: Option<TimekeeperReadBase>,
    raw: Option<TimekeeperReadBase>,
    realtime_offset_ns: i128,
    boottime_offset_ns: u64,
    clocksource_generation: u64,
}

impl TimekeeperData {
    const fn new() -> Self {
        Self {
            mono: None,
            raw: None,
            realtime_offset_ns: 0,
            boottime_offset_ns: 0,
            clocksource_generation: 0,
        }
    }

    fn forward(&mut self) {
        let Some(mono) = self.mono.as_mut() else {
            return;
        };
        let raw = self.raw.as_mut().expect("raw timekeeper base missing");
        debug_assert!(Arc::ptr_eq(&mono.clock, &raw.clock));
        debug_assert_eq!(mono.cycle_last, raw.cycle_last);
        let cycle_now = mono.clock.read().data();
        mono.forward(cycle_now);
        raw.forward(cycle_now);
        debug_assert_eq!(mono.cycle_last, raw.cycle_last);
    }

    fn monotonic_ns(&self) -> u64 {
        let mono = self.mono.as_ref().expect("timekeeper not initialized");
        mono.read_ns(mono.clock.read().data())
    }

    fn raw_ns(&self) -> u64 {
        let raw = self.raw.as_ref().expect("raw timekeeper not initialized");
        raw.read_ns(raw.clock.read().data())
    }

    fn boottime_ns(&self) -> u64 {
        self.monotonic_ns()
            .checked_add(self.boottime_offset_ns)
            .expect("boottime overflow")
    }

    fn realtime_ns(&self) -> i128 {
        self.monotonic_ns() as i128 + self.realtime_offset_ns
    }
}

#[derive(Debug)]
pub struct Timekeeper {
    inner: RwLock<TimekeeperData>,
}

impl Timekeeper {
    const fn new() -> Self {
        Self {
            inner: RwLock::new(TimekeeperData::new()),
        }
    }

    pub fn timekeeper_setup_internals(
        &self,
        clock: Arc<dyn Clocksource>,
    ) -> Result<(), SystemError> {
        let data = clock.clocksource_data();
        if data.mask.bits() == 0
            || data.mult == 0
            || data.shift >= u64::BITS
            || data.max_cycles == 0
            || data.max_cycles > data.mask.bits()
            || !mask_is_contiguous(data.mask.bits())
        {
            warn!(
                "refusing invalid clocksource {}: mask={:#x} mult={} shift={}",
                data.name,
                data.mask.bits(),
                data.mult,
                data.shift
            );
            return Err(SystemError::EINVAL);
        }

        let mut tk = self.inner.write_irqsave();
        if let Some(current) = tk.mono.as_ref() {
            if Arc::ptr_eq(&current.clock, &clock) {
                return Ok(());
            }
            tk.forward();
        }

        let mono_base_ns = tk.mono.as_ref().map_or(0, |base| base.base_ns);
        let raw_base_ns = tk.raw.as_ref().map_or(0, |base| base.base_ns);
        let old_mono_fraction = tk.mono.as_ref().map_or(0, |base| base.fraction);
        let old_raw_fraction = tk.raw.as_ref().map_or(0, |base| base.fraction);
        let old_shift = tk.mono.as_ref().map(|base| base.shift);

        let cycle_last = clock.read().data();
        let mut mono = TimekeeperReadBase::new(clock.clone(), &data, cycle_last, mono_base_ns)
            .expect("validated clocksource became invalid");
        let mut raw = TimekeeperReadBase::new(clock, &data, cycle_last, raw_base_ns)
            .expect("validated clocksource became invalid");
        if let Some(old_shift) = old_shift {
            mono.fraction = rescale_fraction(old_mono_fraction, old_shift, mono.shift);
            raw.fraction = rescale_fraction(old_raw_fraction, old_shift, raw.shift);
        }
        tk.mono = Some(mono);
        tk.raw = Some(raw);
        tk.clocksource_generation = tk.clocksource_generation.wrapping_add(1);
        Ok(())
    }

    pub fn current_clocksource(&self) -> Option<Arc<dyn Clocksource>> {
        self.inner
            .read_irqsave()
            .mono
            .as_ref()
            .map(|base| base.clock.clone())
    }
}

#[inline]
const fn mask_is_contiguous(mask: u64) -> bool {
    mask != 0 && (mask & mask.wrapping_add(1)) == 0
}

#[inline]
fn clocksource_delta(
    cycle_now: u64,
    cycle_last: u64,
    mask: u64,
    max_cycles: u64,
) -> Result<u64, SystemError> {
    if !mask_is_contiguous(mask) || max_cycles == 0 || max_cycles > mask {
        return Err(SystemError::EINVAL);
    }
    let delta = cycle_now.wrapping_sub(cycle_last) & mask;
    // Match Linux timekeeping_get_delta(): a delayed update beyond the
    // conversion-safe window is capped, not allowed to overflow and not
    // promoted into a kernel panic. The next writer advances cycle_last to
    // the current sample, so the exceptional interval cannot repeat.
    Ok(delta.min(max_cycles))
}

#[inline]
const fn fraction_mask(shift: u32) -> u128 {
    if shift == 0 {
        0
    } else {
        (1u128 << shift) - 1
    }
}

#[inline]
fn scale_delta(
    delta: u64,
    mult: u32,
    shift: u32,
    fraction: u64,
) -> Result<(u64, u64), SystemError> {
    if mult == 0 || shift >= u64::BITS || fraction as u128 > fraction_mask(shift) {
        return Err(SystemError::EINVAL);
    }
    let scaled = delta as u128 * mult as u128 + fraction as u128;
    let elapsed = (scaled >> shift)
        .try_into()
        .map_err(|_| SystemError::EOVERFLOW)?;
    Ok((elapsed, (scaled & fraction_mask(shift)) as u64))
}

#[inline]
fn add_elapsed(base_ns: u64, elapsed_ns: u64) -> Result<u64, SystemError> {
    base_ns
        .checked_add(elapsed_ns)
        .ok_or(SystemError::EOVERFLOW)
}

#[inline]
fn rescale_fraction(fraction: u64, old_shift: u32, new_shift: u32) -> u64 {
    if new_shift >= old_shift {
        fraction
            .checked_shl(new_shift - old_shift)
            .expect("timekeeper fraction shift overflow")
    } else {
        fraction >> (old_shift - new_shift)
    }
}

#[inline]
fn ns_to_timespec(ns: i128) -> PosixTimeSpec {
    let sec = ns.div_euclid(NSEC_PER_SEC as i128);
    let nsec = ns.rem_euclid(NSEC_PER_SEC as i128);
    PosixTimeSpec {
        tv_sec: sec.try_into().expect("timekeeper seconds overflow"),
        tv_nsec: nsec as i64,
    }
}

#[inline(always)]
pub fn timekeeper() -> &'static Timekeeper {
    unsafe { __TIMEKEEPER.as_ref().unwrap() }
}

pub fn boottime_seconds() -> i64 {
    let tk = timekeeper().inner.read_irqsave();
    tk.realtime_offset_ns
        .checked_sub(tk.boottime_offset_ns as i128)
        .expect("boot epoch overflow")
        .div_euclid(NSEC_PER_SEC as i128)
        .try_into()
        .expect("boot epoch seconds overflow")
}

pub fn timekeeping_is_initialized() -> bool {
    unsafe { __TIMEKEEPER.is_some() }
}

pub fn timekeeper_init() {
    unsafe { __TIMEKEEPER = Some(Timekeeper::new()) };
}

pub fn realtime_now() -> PosixTimeSpec {
    let tk = timekeeper().inner.read_irqsave();
    ns_to_timespec(tk.realtime_ns())
}

pub fn monotonic_now() -> PosixTimeSpec {
    let tk = timekeeper().inner.read_irqsave();
    ns_to_timespec(tk.monotonic_ns() as i128)
}

pub fn monotonic_raw_now() -> PosixTimeSpec {
    let tk = timekeeper().inner.read_irqsave();
    ns_to_timespec(tk.raw_ns() as i128)
}

pub fn boottime_now() -> PosixTimeSpec {
    let tk = timekeeper().inner.read_irqsave();
    ns_to_timespec(tk.boottime_ns() as i128)
}

pub fn realtime_coarse() -> PosixTimeSpec {
    let tk = timekeeper().inner.read_irqsave();
    let mono = tk.mono.as_ref().expect("timekeeper not initialized");
    ns_to_timespec(mono.base_ns as i128 + tk.realtime_offset_ns)
}

pub fn monotonic_coarse() -> PosixTimeSpec {
    let tk = timekeeper().inner.read_irqsave();
    let mono = tk.mono.as_ref().expect("timekeeper not initialized");
    ns_to_timespec(mono.base_ns as i128)
}

pub fn getnstimeofday() -> PosixTimeSpec {
    realtime_now()
}

pub fn do_gettimeofday() -> PosixTimeval {
    let tp = realtime_now();
    PosixTimeval {
        tv_sec: tp.tv_sec,
        tv_usec: (tp.tv_nsec / 1000) as i32,
    }
}

pub fn do_settimeofday64(time: PosixTimeSpec) -> Result<(), SystemError> {
    let requested = validate_settimeofday(time)?;
    let mut tk = timekeeper().inner.write_irqsave();
    settimeofday_locked(&mut tk, requested)
}

fn validate_settimeofday(time: PosixTimeSpec) -> Result<i128, SystemError> {
    if time.tv_sec < 0
        || time.tv_sec >= TIME_SETTOD_SEC_MAX
        || time.tv_nsec < 0
        || time.tv_nsec >= NSEC_PER_SEC as i64
    {
        return Err(SystemError::EINVAL);
    }

    Ok(time.tv_sec as i128 * NSEC_PER_SEC as i128 + time.tv_nsec as i128)
}

/// Apply a validated wall-clock value while the caller owns the timekeeper
/// write side.  As in Linux, forwarding happens before rejecting a formatted
/// value that would place realtime before monotonic; only the wall offset is
/// transactional on that error path.
fn settimeofday_locked(tk: &mut TimekeeperData, requested: i128) -> Result<(), SystemError> {
    tk.forward();
    let monotonic = tk
        .mono
        .as_ref()
        .expect("timekeeper not initialized")
        .base_ns as i128;
    let realtime_offset = requested
        .checked_sub(monotonic)
        .ok_or(SystemError::EOVERFLOW)?;
    // Linux rejects only wall-clock values earlier than CLOCK_MONOTONIC.
    // CLOCK_BOOTTIME may legitimately be ahead after suspend.
    if realtime_offset < 0 {
        return Err(SystemError::EINVAL);
    }
    // Prove all derived public values are representable before publishing the
    // offset.  `requested` was already bounded by validate_settimeofday().
    let _boot_epoch = realtime_offset
        .checked_sub(tk.boottime_offset_ns as i128)
        .ok_or(SystemError::EOVERFLOW)?;
    tk.realtime_offset_ns = realtime_offset;
    Ok(())
}

#[inline(never)]
pub fn timekeeping_init() {
    info!("Initializing timekeeping module...");
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
    timekeeper_init();

    // Registration computes max_cycles/max_idle_ns.  The default source must
    // be fully registered before its conversion snapshot is installed, so
    // non-KVM/TCG boots never depend on a later source replacing it.
    jiffies_init();

    let clock = clocksource_default_clock();
    clock
        .enable()
        .expect("clocksource_default_clock enable failed");
    timekeeper()
        .timekeeper_setup_internals(clock)
        .expect("default clocksource has invalid conversion data");

    let initial_realtime = ktime_get_real_ns();
    if initial_realtime > 0 {
        do_settimeofday64(ns_to_timespec(initial_realtime as i128))
            .expect("initial realtime is invalid");
    }

    drop(irq_guard);
    info!("timekeeping_init successfully");
}

pub fn update_wall_time() {
    if TIMEKEEPING_SUSPENDED.load(Ordering::Acquire) {
        return;
    }
    timekeeper().inner.write_irqsave().forward();
}

#[path = "timekeeping_selftest.rs"]
mod selftest;

pub(crate) use selftest::run_timekeeping_selftests;
