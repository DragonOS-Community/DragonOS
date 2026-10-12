//! Linux 6.6 page-counter size parsing, including its unsigned wrap semantics.
//!
//! `page_counter_memparse()` uses `memparse()` / `simple_strtoull()`, not
//! `kstrtoull()`: overflow is intentionally not an ERANGE error. Keep this
//! parser local to memory controller files rather than changing other ABIs.

use system_error::SystemError;

use crate::{arch::MMArch, mm::MemoryManagementArch};

pub(crate) use super::accounting::PAGE_COUNTER_MAX;

/// Parse a min/low/high/max value into physical pages. `max` is the sentinel.
pub(crate) fn parse_limit_pages(input: &str) -> Result<u64, SystemError> {
    parse_pages(input, b"max")
}

/// Parse memory.reclaim. Linux 6.6 accepts no `max` or swappiness option.
pub(crate) fn parse_reclaim_pages(input: &str) -> Result<u64, SystemError> {
    // The Linux caller passes an empty max marker. Consequently an empty,
    // stripped string selects PAGE_COUNTER_MAX even for memory.reclaim.
    parse_pages(input, b"")
}

/// memory.oom.group uses kstrtoint(base 0), not wrapping memparse().
pub(crate) fn parse_oom_group(input: &str) -> Result<bool, SystemError> {
    let bytes = strip_c_string(input);
    let (negative, bytes) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let (radix, mut offset) = radix(bytes);
    let first_digit = offset;
    let mut value = 0u64;
    let mut overflow = false;
    while let Some(next) = bytes.get(offset).and_then(|c| digit(*c)) {
        if next >= radix {
            break;
        }
        let next = next as u64;
        overflow |= value
            .checked_mul(radix as u64)
            .and_then(|value| value.checked_add(next))
            .is_none();
        value = value.wrapping_mul(radix as u64).wrapping_add(next);
        offset += 1;
    }
    // kstrtoull checks u64 overflow before trailing characters; kstrtoint
    // checks its narrower range only after the string is otherwise valid.
    if overflow {
        return Err(SystemError::ERANGE);
    }
    if offset == first_digit || offset != bytes.len() {
        return Err(SystemError::EINVAL);
    }
    let maximum = i32::MAX as u64 + u64::from(negative);
    if value > maximum {
        return Err(SystemError::ERANGE);
    }
    match (negative, value) {
        (_, 0) => Ok(false),
        (false, 1) => Ok(true),
        _ => Err(SystemError::EINVAL),
    }
}

fn strip_c_string(input: &str) -> &[u8] {
    // kernfs supplies a NUL-terminated C string; strstrip() uses ASCII isspace.
    input
        .split('\0')
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'))
        .as_bytes()
}

fn radix(bytes: &[u8]) -> (u8, usize) {
    if bytes.first() == Some(&b'0') {
        if bytes.get(1).is_some_and(|c| *c == b'x' || *c == b'X')
            && bytes.get(2).and_then(|c| digit(*c)).is_some()
        {
            (16, 2)
        } else {
            (8, 0)
        }
    } else {
        (10, 0)
    }
}

fn parse_pages(input: &str, max_marker: &[u8]) -> Result<u64, SystemError> {
    let bytes = strip_c_string(input);
    if bytes == max_marker {
        return Ok(PAGE_COUNTER_MAX);
    }

    let (radix, mut offset) = radix(bytes);
    let mut value = 0u64;
    while let Some(next) = bytes.get(offset).and_then(|c| digit(*c)) {
        if next >= radix {
            break;
        }
        value = value.wrapping_mul(radix as u64).wrapping_add(next as u64);
        offset += 1;
    }

    if let Some(shift) = bytes.get(offset).and_then(|suffix| match suffix {
        b'k' | b'K' => Some(10),
        b'm' | b'M' => Some(20),
        b'g' | b'G' => Some(30),
        b't' | b'T' => Some(40),
        b'p' | b'P' => Some(50),
        b'e' | b'E' => Some(60),
        _ => None,
    }) {
        value = value.wrapping_shl(shift);
        offset += 1;
    }
    if offset != bytes.len() {
        return Err(SystemError::EINVAL);
    }
    Ok((value / MMArch::PAGE_SIZE as u64).min(PAGE_COUNTER_MAX))
}

fn digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oom_group_uses_signed_checked_integer_not_memparse() {
        for input in ["0", "-0", "+0", "00", "0x0", " \x0b0\n"] {
            assert_eq!(parse_oom_group(input), Ok(false), "{input:?}");
        }
        for input in ["1", "+1", "01", "0X1", "1\0ignored"] {
            assert_eq!(parse_oom_group(input), Ok(true), "{input:?}");
        }
        for input in ["", "-1", "2", "1K", "08", "0x", "-+1", "1\n2"] {
            assert_eq!(parse_oom_group(input), Err(SystemError::EINVAL));
        }
        for input in ["2147483648", "-2147483649", "18446744073709551616junk"] {
            assert_eq!(parse_oom_group(input), Err(SystemError::ERANGE));
        }
        assert_eq!(parse_oom_group("2147483648junk"), Err(SystemError::EINVAL));
        assert_eq!(parse_oom_group("-2147483648"), Err(SystemError::EINVAL));
    }

    #[test]
    fn limits_use_base_zero_and_round_down_to_pages() {
        let page = MMArch::PAGE_SIZE as u64;
        assert_eq!(parse_limit_pages("4095"), Ok(4095 / page));
        assert_eq!(parse_limit_pages("4097"), Ok(4097 / page));
        assert_eq!(parse_limit_pages("0x1000"), Ok(4096 / page));
        assert_eq!(parse_limit_pages("0X1000"), Ok(4096 / page));
        assert_eq!(parse_limit_pages("010000"), Ok(4096 / page));
        assert_eq!(parse_limit_pages("\t 4K\n"), Ok(4096 / page));
        assert_eq!(parse_limit_pages("\x0b4K\x0c"), Ok(4096 / page));
        assert_eq!(parse_limit_pages("0x10K"), Ok(16384 / page));
        assert_eq!(parse_limit_pages("1m"), Ok((1 << 20) / page));
        assert_eq!(parse_limit_pages("1G"), Ok((1 << 30) / page));
        assert_eq!(parse_limit_pages("1t"), Ok((1 << 40) / page));
        assert_eq!(parse_limit_pages("1P"), Ok((1 << 50) / page));
        assert_eq!(parse_limit_pages("1e"), Ok((1 << 60) / page));
        // E/e is a hex digit when consumed by the integer parser.
        assert_eq!(parse_limit_pages("0x1E"), Ok(30 / page));
    }

    #[test]
    fn parser_preserves_unsigned_wrap_before_page_counter_cap() {
        assert_eq!(
            parse_limit_pages("18446744073709551615"),
            Ok(PAGE_COUNTER_MAX)
        );
        assert_eq!(parse_limit_pages("18446744073709551616"), Ok(0));
        assert_eq!(
            parse_limit_pages("18446744073709555712"),
            Ok(4096 / MMArch::PAGE_SIZE as u64)
        );
        assert_eq!(parse_limit_pages("0x10000000000000000"), Ok(0));
        assert_eq!(parse_limit_pages("16E"), Ok(0));
        assert_eq!(parse_limit_pages("8E"), Ok(PAGE_COUNTER_MAX));
        assert_eq!(parse_limit_pages("18446744073709551616K"), Ok(0));
    }

    #[test]
    fn max_and_empty_inputs_match_the_linux_callers() {
        assert_eq!(parse_limit_pages(" max\n"), Ok(PAGE_COUNTER_MAX));
        assert_eq!(parse_limit_pages(""), Ok(0));
        assert_eq!(parse_limit_pages("\t\n"), Ok(0));
        assert_eq!(parse_reclaim_pages(""), Ok(PAGE_COUNTER_MAX));
        assert_eq!(parse_reclaim_pages("\t\n"), Ok(PAGE_COUNTER_MAX));
        assert_eq!(parse_reclaim_pages("max"), Err(SystemError::EINVAL));
        // simple_strtoull consumes no digits, but memparse consumes the suffix.
        assert_eq!(parse_limit_pages("K"), Ok(0));
        assert_eq!(parse_reclaim_pages("E"), Ok(0));
        assert_eq!(
            parse_limit_pages("4096\0ignored"),
            Ok(4096 / MMArch::PAGE_SIZE as u64)
        );
    }

    #[test]
    fn unconsumed_text_is_invalid_not_erange() {
        for input in [
            "+4096",
            "-4096",
            "08",
            "0x",
            "0xg",
            "4KB",
            "4K 0",
            "4 K",
            "MAX",
            "1M\u{00a0}",
        ] {
            assert_eq!(
                parse_limit_pages(input),
                Err(SystemError::EINVAL),
                "{input}"
            );
        }
        assert_eq!(
            parse_reclaim_pages("4K swappiness=0"),
            Err(SystemError::EINVAL)
        );
        assert_eq!(
            parse_reclaim_pages("4097"),
            Ok(4097 / MMArch::PAGE_SIZE as u64)
        );
    }
}
