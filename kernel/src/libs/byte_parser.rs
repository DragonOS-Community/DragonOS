//! Byte slice parsing utilities.
//!
//! Provides functions for parsing various data types from byte slices,
//! commonly used for system call arguments and socket options.

use system_error::SystemError;

/// Reads a `i32` value from the beginning of a byte slice.
///
/// Returns `EINVAL` if the slice is too short.
pub fn read_i32(val: &[u8]) -> Result<i32, SystemError> {
    if val.len() < 4 {
        return Err(SystemError::EINVAL);
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&val[..4]);
    Ok(i32::from_ne_bytes(bytes))
}

/// Reads a `u32` value from the beginning of a byte slice.
///
/// Returns `EINVAL` if the slice is too short.
pub fn read_u32(val: &[u8]) -> Result<u32, SystemError> {
    if val.len() < 4 {
        return Err(SystemError::EINVAL);
    }
    let mut bytes = [0u8; 4];
    bytes.copy_from_slice(&val[..4]);
    Ok(u32::from_ne_bytes(bytes))
}

/// Reads a `i64` value from the beginning of a byte slice.
///
/// Returns `EINVAL` if the slice is too short.
#[allow(dead_code)]
pub fn read_i64(val: &[u8]) -> Result<i64, SystemError> {
    if val.len() < 8 {
        return Err(SystemError::EINVAL);
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&val[..8]);
    Ok(i64::from_ne_bytes(bytes))
}

/// Reads a `u64` value from the beginning of a byte slice.
///
/// Returns `EINVAL` if the slice is too short.
#[allow(dead_code)]
pub fn read_u64(val: &[u8]) -> Result<u64, SystemError> {
    if val.len() < 8 {
        return Err(SystemError::EINVAL);
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&val[..8]);
    Ok(u64::from_ne_bytes(bytes))
}

/// Reads a boolean flag from a byte slice.
///
/// - If at least 4 bytes are available: interprets as i32 (nonzero = true)
/// - Otherwise: uses the first byte (nonzero = true)
///
/// Returns `EINVAL` if the slice is empty.
pub fn read_bool_flag(val: &[u8]) -> Result<bool, SystemError> {
    if val.is_empty() {
        return Err(SystemError::EINVAL);
    }
    Ok(if val.len() >= 4 {
        read_i32(val)? != 0
    } else {
        val[0] != 0
    })
}

/// Reads a string from a byte slice, trimming null terminator bytes.
///
/// Returns `EINVAL` if the bytes are not valid UTF-8.
pub fn read_string(val: &[u8]) -> Result<&str, SystemError> {
    core::str::from_utf8(val)
        .map_err(|_| SystemError::EINVAL)
        .map(|s| s.trim_matches(char::from(0)))
}
