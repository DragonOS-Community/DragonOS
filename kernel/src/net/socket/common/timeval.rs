use system_error::SystemError;

/// Parse timeval-like socket option payload.
///
/// Accepts 64-bit or 32-bit timeval layouts and returns:
/// - Ok(None): zero timeout (0,0)
/// - Ok(Some(Duration)): positive/negative sec per Linux semantics
pub fn parse_timeval_opt(optval: &[u8]) -> Result<Option<crate::time::Duration>, SystemError> {
    // 64-bit timeval: 8-byte sec + 8-byte usec
    if optval.len() >= 16 {
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
            return Ok(Some(crate::time::Duration::from_micros(0)));
        }
        if sec == 0 && usec == 0 {
            return Ok(None);
        }
        let total_us = (sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(usec as u64);
        return Ok(Some(crate::time::Duration::from_micros(total_us)));
    }

    // 32-bit timeval: 8-byte sec + 4-byte usec
    if optval.len() >= 12 {
        let mut sec_raw = [0u8; 8];
        let mut usec_raw = [0u8; 4];
        sec_raw.copy_from_slice(&optval[..8]);
        usec_raw.copy_from_slice(&optval[8..12]);
        let sec = i64::from_ne_bytes(sec_raw);
        let usec = i32::from_ne_bytes(usec_raw) as i64;
        if !(0..1_000_000).contains(&usec) {
            return Err(SystemError::EDOM);
        }
        if sec < 0 {
            return Ok(Some(crate::time::Duration::from_micros(0)));
        }
        if sec == 0 && usec == 0 {
            return Ok(None);
        }
        let total_us = (sec as u64)
            .saturating_mul(1_000_000)
            .saturating_add(usec as u64);
        return Ok(Some(crate::time::Duration::from_micros(total_us)));
    }

    Err(SystemError::EINVAL)
}

/// Write timeval-like socket option payload (64-bit timeval layout).
pub fn write_timeval_opt(value: &mut [u8], micros: u64) -> Result<usize, SystemError> {
    if value.len() < 16 {
        return Err(SystemError::EINVAL);
    }
    let sec = (micros / 1_000_000) as i64;
    let usec = (micros % 1_000_000) as i64;
    value[..8].copy_from_slice(&sec.to_ne_bytes());
    value[8..16].copy_from_slice(&usec.to_ne_bytes());
    Ok(16)
}
