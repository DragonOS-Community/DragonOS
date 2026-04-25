use core::sync::atomic::{AtomicIsize, AtomicU64, AtomicUsize, Ordering};

use crate::time::{clocksource::HZ, timer::clock};

use super::CpuRunQueue;

pub const FSHIFT: u32 = 11;
pub const FIXED_1: u64 = 1 << FSHIFT;
pub const LOAD_FREQ: u64 = 5 * HZ + 1;

const EXP_1: u64 = 1884;
const EXP_5: u64 = 2014;
const EXP_15: u64 = 2037;

static CALC_LOAD_TASKS: AtomicIsize = AtomicIsize::new(0);
static CALC_LOAD_UPDATE: AtomicU64 = AtomicU64::new(0);
static AVENRUN: [AtomicU64; 3] = [AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)];
static NR_RUNNING: AtomicUsize = AtomicUsize::new(0);
static NR_UNINTERRUPTIBLE: AtomicUsize = AtomicUsize::new(0);

fn calc_load(load: u64, exp: u64, active: u64) -> u64 {
    let mut newload: u128 =
        (load as u128) * (exp as u128) + (active as u128) * ((FIXED_1 - exp) as u128);
    if active >= load {
        newload += (FIXED_1 - 1) as u128;
    }
    (newload / (FIXED_1 as u128)) as u64
}

fn fixed_power_int(mut x: u64, frac_bits: u32, mut n: u64) -> u64 {
    let mut result: u128 = 1u128 << frac_bits;
    let frac_bits_u128 = frac_bits as u128;
    let rounding: u128 = 1u128 << (frac_bits - 1);

    if n == 0 {
        return result as u64;
    }

    loop {
        if (n & 1) != 0 {
            result = (result * (x as u128) + rounding) >> frac_bits_u128;
        }
        n >>= 1;
        if n == 0 {
            break;
        }
        x = (((x as u128) * (x as u128) + rounding) >> frac_bits_u128) as u64;
    }

    result as u64
}

fn calc_load_n(load: u64, exp: u64, active: u64, n: u64) -> u64 {
    calc_load(load, fixed_power_int(exp, FSHIFT, n), active)
}

pub fn get_avenrun(offset: u64, shift: u32) -> [u64; 3] {
    [
        (AVENRUN[0].load(Ordering::Relaxed) + offset) << shift,
        (AVENRUN[1].load(Ordering::Relaxed) + offset) << shift,
        (AVENRUN[2].load(Ordering::Relaxed) + offset) << shift,
    ]
}

pub fn nr_running() -> u32 {
    NR_RUNNING.load(Ordering::Relaxed) as u32
}

pub fn nr_uninterruptible() -> u32 {
    NR_UNINTERRUPTIBLE.load(Ordering::Relaxed) as u32
}

pub(super) fn inc_nr_running(delta: usize) {
    NR_RUNNING.fetch_add(delta, Ordering::Relaxed);
}

pub(super) fn dec_nr_running(delta: usize) {
    NR_RUNNING.fetch_sub(delta, Ordering::Relaxed);
}

pub(super) fn inc_nr_uninterruptible(delta: usize) {
    NR_UNINTERRUPTIBLE.fetch_add(delta, Ordering::Relaxed);
}

pub(super) fn dec_nr_uninterruptible(delta: usize) {
    NR_UNINTERRUPTIBLE.fetch_sub(delta, Ordering::Relaxed);
}

pub fn calc_global_load_tick(rq: &mut CpuRunQueue) {
    let now = clock();
    if now < rq.calc_load_update {
        return;
    }

    let nr_active = rq.nr_running as isize + rq.nr_uninterruptible as isize;
    let delta = nr_active - rq.calc_load_active;
    if delta != 0 {
        CALC_LOAD_TASKS.fetch_add(delta, Ordering::SeqCst);
        rq.calc_load_active = nr_active;
    }

    rq.calc_load_update = rq.calc_load_update.saturating_add(LOAD_FREQ);
}

pub fn calc_global_load(now_jiffies: u64) {
    let mut sample_window = CALC_LOAD_UPDATE.load(Ordering::SeqCst);
    if sample_window == 0 {
        let _ = CALC_LOAD_UPDATE.compare_exchange(
            0,
            now_jiffies.saturating_add(LOAD_FREQ),
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        return;
    }

    if now_jiffies < sample_window.saturating_add(10) {
        return;
    }

    let delta = now_jiffies - sample_window - 10;
    let n = 1 + (delta / LOAD_FREQ);

    let active_tasks = CALC_LOAD_TASKS.load(Ordering::SeqCst);
    let active = if active_tasks > 0 {
        (active_tasks as u64).saturating_mul(FIXED_1)
    } else {
        let running = nr_running() as u64;
        let uninterruptible = nr_uninterruptible() as u64;
        (running.saturating_add(uninterruptible)).saturating_mul(FIXED_1)
    };

    let a0 = AVENRUN[0].load(Ordering::SeqCst);
    let a1 = AVENRUN[1].load(Ordering::SeqCst);
    let a2 = AVENRUN[2].load(Ordering::SeqCst);

    AVENRUN[0].store(calc_load_n(a0, EXP_1, active, n), Ordering::SeqCst);
    AVENRUN[1].store(calc_load_n(a1, EXP_5, active, n), Ordering::SeqCst);
    AVENRUN[2].store(calc_load_n(a2, EXP_15, active, n), Ordering::SeqCst);

    sample_window = sample_window.saturating_add(n.saturating_mul(LOAD_FREQ));
    CALC_LOAD_UPDATE.store(sample_window, Ordering::SeqCst);
}
