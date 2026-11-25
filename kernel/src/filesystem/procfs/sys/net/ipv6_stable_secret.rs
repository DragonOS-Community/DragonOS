use core::{net::Ipv6Addr, str};

use system_error::SystemError;

use crate::filesystem::procfs::utils::proc_read;

pub(super) const MODE: u32 = 0o600;
const TEXT_LEN: usize = 39;
const MAX_WRITE_LEN: usize = 45;

fn format_secret(secret: [u8; 16]) -> [u8; TEXT_LEN + 1] {
    let mut text = [b':'; TEXT_LEN + 1];
    let hex = b"0123456789abcdef";
    for (index, byte) in secret.into_iter().enumerate() {
        let at = index * 2 + index / 2;
        text[at] = hex[usize::from(byte >> 4)];
        text[at + 1] = hex[usize::from(byte & 15)];
    }
    text[TEXT_LEN] = b'\n';
    text
}

pub(super) fn read(
    secret: Option<[u8; 16]>,
    offset: usize,
    len: usize,
    buf: &mut [u8],
) -> Result<usize, SystemError> {
    let text = format_secret(secret.ok_or(SystemError::EIO)?);
    proc_read(offset, len, buf, &text)
}

pub(super) fn parse_write(
    previous: Option<[u8; 16]>,
    offset: usize,
    input: &[u8],
) -> Result<Option<[u8; 16]>, SystemError> {
    if input.is_empty() {
        return Ok(None);
    }
    let previous = previous.unwrap_or_default();
    if offset > TEXT_LEN {
        return Ok(Some(previous));
    }
    let mut text = [0; MAX_WRITE_LEN];
    text[..offset].copy_from_slice(&format_secret(previous)[..offset]);
    let added = input
        .iter()
        .take(MAX_WRITE_LEN - offset)
        .take_while(|&&byte| byte != b'\n' && byte != 0)
        .count();
    text[offset..offset + added].copy_from_slice(&input[..added]);
    // Linux in6_pton with delim = -1 accepts a suffix after the address.
    let end = text[..offset + added]
        .iter()
        .take_while(|&&byte| byte.is_ascii_hexdigit() || matches!(byte, b':' | b'.'))
        .count();
    let parsed = str::from_utf8(&text[..end])
        .ok()
        .and_then(|address| address.parse::<Ipv6Addr>().ok())
        .ok_or(SystemError::EIO)?;
    Ok(Some(parsed.octets()))
}
