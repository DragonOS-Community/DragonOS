use system_error::SystemError;

use crate::time::clocksource::HZ;

pub const INFINITE_TIMEOUT_TICKS: u64 = i64::MAX as u64;

/// Parse a native socket timeval into Linux-style scheduler ticks.
pub fn parse_timeval_ticks(optval: &[u8]) -> Result<Option<u64>, SystemError> {
    if optval.len() < 16 {
        return Err(SystemError::EINVAL);
    }

    let mut sec_raw = [0u8; 8];
    let mut usec_raw = [0u8; 8];
    sec_raw.copy_from_slice(&optval[..8]);
    usec_raw.copy_from_slice(&optval[8..16]);
    let sec = i64::from_ne_bytes(sec_raw);
    let usec = i64::from_ne_bytes(usec_raw);
    if !(0..1_000_000).contains(&usec) {
        return Err(SystemError::EDOM);
    }
    if sec < 0 {
        return Ok(Some(0));
    }
    if sec == 0 && usec == 0 {
        return Ok(None);
    }

    let max_finite_sec = INFINITE_TIMEOUT_TICKS / HZ - 1;
    if sec as u64 >= max_finite_sec {
        return Ok(None);
    }
    let subsec_ticks = ((usec as u128 * HZ as u128).div_ceil(1_000_000)) as u64;
    Ok(Some((sec as u64) * HZ + subsec_ticks))
}

pub fn timeout_ticks_to_micros(ticks: u64) -> u64 {
    ((ticks as u128 * 1_000_000) / HZ as u128).min(u64::MAX as u128) as u64
}

pub fn write_timeval_ticks(value: &mut [u8], ticks: u64) -> usize {
    let (sec, usec) = if ticks == INFINITE_TIMEOUT_TICKS {
        (0i64, 0i64)
    } else {
        ((ticks / HZ) as i64, ((ticks % HZ) * 1_000_000 / HZ) as i64)
    };
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&sec.to_ne_bytes());
    buf[8..].copy_from_slice(&usec.to_ne_bytes());
    let len = core::cmp::min(value.len(), buf.len());
    value[..len].copy_from_slice(&buf[..len]);
    len
}

/// Parse timeval-like socket option payload.
///
/// Accepts the native 64-bit timeval layout and returns:
/// - Ok(None): zero timeout (0,0)
/// - Ok(Some(Duration)): positive/negative sec per Linux semantics
pub fn parse_timeval_opt(optval: &[u8]) -> Result<Option<crate::time::Duration>, SystemError> {
    Ok(parse_timeval_ticks(optval)?
        .map(|ticks| crate::time::Duration::from_micros(timeout_ticks_to_micros(ticks))))
}

/// Write timeval-like socket option payload (64-bit timeval layout, truncated to `value.len()`).
pub fn write_timeval_opt(value: &mut [u8], micros: u64) -> usize {
    if value.is_empty() {
        return 0;
    }
    let sec = (micros / 1_000_000) as i64;
    let usec = (micros % 1_000_000) as i64;
    let mut buf = [0u8; 16];
    buf[..8].copy_from_slice(&sec.to_ne_bytes());
    buf[8..16].copy_from_slice(&usec.to_ne_bytes());
    let len = core::cmp::min(value.len(), buf.len());
    value[..len].copy_from_slice(&buf[..len]);
    len
}
