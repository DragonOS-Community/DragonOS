//! Shared parsing for one Linux-style numeric proc sysctl value.

use system_error::SystemError;

/// Linux consumes leading whitespace, one base-0 signed integer and the
/// whitespace immediately following it. Later tokens are left for a short write.
pub(super) fn parse_numeric_sysctl(buf: &[u8]) -> Result<(i64, usize), SystemError> {
    use crate::{arch::MMArch, mm::MemoryManagementArch};

    // __do_proc_dointvec parses at most PAGE_SIZE-1 bytes. Linux still
    // computes the short-write count from the original requested length.
    let parsed_len = buf.len().min(MMArch::PAGE_SIZE - 1);
    let excess = buf.len() - parsed_len;
    let buf = &buf[..parsed_len];
    let mut cursor = 0;
    while cursor < buf.len() && buf[cursor].is_ascii_whitespace() {
        cursor += 1;
    }

    let token_start = cursor;
    let negative = if buf.get(cursor) == Some(&b'-') {
        cursor += 1;
        true
    } else {
        false
    };
    let digits_start = cursor;
    let (radix, prefix_len) = if buf.get(cursor) == Some(&b'0')
        && matches!(buf.get(cursor + 1), Some(b'x' | b'X'))
        && buf
            .get(cursor + 2)
            .is_some_and(|byte| byte.is_ascii_hexdigit())
    {
        (16u32, 2usize)
    } else if buf.get(cursor) == Some(&b'0') {
        (8u32, 0usize)
    } else {
        (10u32, 0usize)
    };
    cursor += prefix_len;
    let number_start = cursor;
    let mut magnitude = 0u64;
    while let Some(byte) = buf.get(cursor) {
        let Some(digit) = (*byte as char).to_digit(radix) else {
            break;
        };
        magnitude = magnitude
            .checked_mul(radix as u64)
            .and_then(|value| value.checked_add(digit as u64))
            .ok_or(SystemError::EINVAL)?;
        cursor += 1;
    }
    if cursor == number_start || (prefix_len == 0 && cursor == digits_start) {
        return Err(SystemError::EINVAL);
    }
    // Linux proc_get_long() uses TMPBUFLEN=22.
    if cursor - token_start >= 21 {
        return Err(SystemError::EINVAL);
    }
    if let Some(byte) = buf.get(cursor) {
        if !matches!(byte, b' ' | b'\t' | b'\n') {
            return Err(SystemError::EINVAL);
        }
    }

    let magnitude = i64::try_from(magnitude).map_err(|_| SystemError::EINVAL)?;
    let value = if negative { -magnitude } else { magnitude };
    while cursor < buf.len() && buf[cursor].is_ascii_whitespace() {
        cursor += 1;
    }
    Ok((value, cursor + excess))
}
