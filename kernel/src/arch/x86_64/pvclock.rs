use core::sync::atomic::{fence, AtomicU64, AtomicU8, Ordering};

use x86::time::rdtsc;

pub const PVCLOCK_TSC_STABLE_BIT: u8 = 1 << 0;
#[allow(dead_code)]
pub const PVCLOCK_GUEST_STOPPED: u8 = 1 << 1;

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PvclockVcpuTimeInfo {
    pub version: u32,
    pub pad0: u32,
    pub tsc_timestamp: u64,
    pub system_time: u64,
    pub tsc_to_system_mul: u32,
    pub tsc_shift: i8,
    pub flags: u8,
    pub pad: [u8; 2],
}

#[allow(dead_code)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PvclockWallClock {
    pub version: u32,
    pub sec: u32,
    pub nsec: u32,
}

#[repr(C, align(64))]
#[derive(Clone, Copy, Debug)]
pub struct PvclockVsyscallTimeInfo {
    pub pvti: PvclockVcpuTimeInfo,
}

static VALID_FLAGS: AtomicU8 = AtomicU8::new(0);
static LAST_VALUE: AtomicU64 = AtomicU64::new(0);

pub fn pvclock_set_flags(flags: u8) {
    VALID_FLAGS.store(flags, Ordering::Relaxed);
}

pub fn pvclock_read_begin(src: &PvclockVcpuTimeInfo) -> u32 {
    let version = unsafe { core::ptr::read_volatile(&src.version) } & !1;
    fence(Ordering::Acquire);
    version
}

pub fn pvclock_read_retry(src: &PvclockVcpuTimeInfo, version: u32) -> bool {
    fence(Ordering::Acquire);
    let cur = unsafe { core::ptr::read_volatile(&src.version as *const u32) };
    cur != version
}

#[allow(dead_code)]
pub fn pvclock_read_flags(src: &PvclockVcpuTimeInfo) -> u8 {
    let mut flags;
    loop {
        let version = pvclock_read_begin(src);
        flags = unsafe { core::ptr::read_volatile(&src.flags as *const u8) };
        if !pvclock_read_retry(src, version) {
            break;
        }
    }

    flags & VALID_FLAGS.load(Ordering::Relaxed)
}

pub fn pvclock_scale_delta(mut delta: u64, mul_frac: u32, shift: i8) -> u64 {
    if shift < 0 {
        delta >>= (-shift) as u32;
    } else {
        delta <<= shift as u32;
    }

    let product = (delta as u128 * mul_frac as u128) >> 32;
    product as u64
}

pub fn pvclock_read_cycles(src: &PvclockVcpuTimeInfo, tsc: u64) -> u64 {
    let tsc_timestamp = unsafe { core::ptr::read_volatile(&src.tsc_timestamp as *const u64) };
    let system_time = unsafe { core::ptr::read_volatile(&src.system_time as *const u64) };
    let tsc_to_system_mul =
        unsafe { core::ptr::read_volatile(&src.tsc_to_system_mul as *const u32) };
    let tsc_shift = unsafe { core::ptr::read_volatile(&src.tsc_shift as *const i8) };

    let delta = tsc.wrapping_sub(tsc_timestamp);
    let offset = pvclock_scale_delta(delta, tsc_to_system_mul, tsc_shift);
    system_time.wrapping_add(offset)
}

pub fn pvclock_clocksource_read_nowd(src: &PvclockVcpuTimeInfo) -> u64 {
    let mut ret;
    let mut flags;

    loop {
        let version = pvclock_read_begin(src);
        ret = pvclock_read_cycles(src, unsafe { rdtsc() });
        flags = unsafe { core::ptr::read_volatile(&src.flags as *const u8) };
        if !pvclock_read_retry(src, version) {
            break;
        }
    }

    let valid = VALID_FLAGS.load(Ordering::Relaxed);
    if (valid & PVCLOCK_TSC_STABLE_BIT) != 0 && (flags & PVCLOCK_TSC_STABLE_BIT) != 0 {
        return ret;
    }

    loop {
        let last = LAST_VALUE.load(Ordering::Relaxed);
        if ret <= last {
            return last;
        }
        if LAST_VALUE
            .compare_exchange(last, ret, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            return ret;
        }
    }
}

pub fn pvclock_tsc_khz(src: &PvclockVcpuTimeInfo) -> u64 {
    let tsc_to_system_mul =
        unsafe { core::ptr::read_volatile(&src.tsc_to_system_mul as *const u32) } as u64;
    let tsc_shift = unsafe { core::ptr::read_volatile(&src.tsc_shift as *const i8) } as i32;

    if tsc_to_system_mul == 0 {
        return 0;
    }

    let mut pv_tsc_khz = 1_000_000u64 << 32;
    pv_tsc_khz /= tsc_to_system_mul;
    if tsc_shift < 0 {
        pv_tsc_khz <<= (-tsc_shift) as u32;
    } else {
        pv_tsc_khz >>= tsc_shift as u32;
    }
    pv_tsc_khz
}
