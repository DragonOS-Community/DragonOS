//! Linux bitmap_parselist syntax, with bounded indices and canonical output.
use alloc::{collections::BTreeSet, format, string::String};
use system_error::SystemError;

pub(super) type IndexSet = BTreeSet<usize>;

fn number(bytes: &[u8], pos: &mut usize, bits: usize) -> Result<u32, SystemError> {
    if bytes.get(*pos) == Some(&b'N') {
        *pos += 1;
        return Ok((bits - 1) as u32);
    }
    let begin = *pos;
    let mut value = 0u32;
    while let Some(c @ b'0'..=b'9') = bytes.get(*pos) {
        value = value
            .checked_mul(10)
            .and_then(|v| v.checked_add((c - b'0') as u32))
            .ok_or(SystemError::EOVERFLOW)?;
        *pos += 1;
    }
    if begin == *pos {
        return Err(SystemError::EINVAL);
    }
    Ok(value)
}

pub(super) fn parse(input: &str, bits: usize) -> Result<IndexSet, SystemError> {
    debug_assert!(bits != 0);
    let bytes = input.as_bytes();
    let mut pos = 0;
    let mut result = IndexSet::new();
    let end = |c: Option<&u8>| c.is_none() || matches!(c, Some(b'\n' | 0));
    let separator = |c: &u8| c.is_ascii_whitespace() || *c == b',';
    loop {
        while bytes.get(pos).is_some_and(separator) {
            pos += 1;
        }
        if end(bytes.get(pos)) {
            return Ok(result);
        }
        let (start, finish, range) = if bytes
            .get(pos..pos + 3)
            .is_some_and(|v| v.eq_ignore_ascii_case(b"all"))
        {
            pos += 3;
            (0, (bits - 1) as u32, true)
        } else {
            let start = number(bytes, &mut pos, bits)?;
            if bytes.get(pos) == Some(&b'-') {
                pos += 1;
                (start, number(bytes, &mut pos, bits)?, true)
            } else {
                (start, start, false)
            }
        };
        let (used, group) = if bytes.get(pos) == Some(&b':') {
            // Linux accepts group patterns after a range (or "all"), not a single index.
            if !range {
                return Err(SystemError::EINVAL);
            }
            pos += 1;
            let used = number(bytes, &mut pos, bits)?;
            if bytes.get(pos) != Some(&b'/') {
                return Err(SystemError::EINVAL);
            }
            pos += 1;
            (used, number(bytes, &mut pos, bits)?)
        } else {
            (finish.saturating_add(1), finish.saturating_add(1))
        };
        if start > finish || group == 0 || used > group {
            return Err(SystemError::EINVAL);
        }
        if finish as usize >= bits {
            return Err(SystemError::ERANGE);
        }
        if !end(bytes.get(pos)) && !bytes.get(pos).is_some_and(separator) {
            return Err(SystemError::EINVAL);
        }
        for index in start..=finish {
            if (index - start) % group < used {
                result.insert(index as usize);
            }
        }
        if end(bytes.get(pos)) {
            return Ok(result);
        }
    }
}

pub(super) fn list(set: &IndexSet) -> String {
    let mut out = String::new();
    let mut iter = set.iter().copied().peekable();
    while let Some(start) = iter.next() {
        let mut end = start;
        while iter.peek() == Some(&(end + 1)) {
            end = iter.next().unwrap();
        }
        if !out.is_empty() {
            out.push(',');
        }
        out.push_str(&format!("{}", start));
        if end != start {
            out.push_str(&format!("-{}", end));
        }
    }
    out
}
