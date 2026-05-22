//! Linux-aligned getsockopt value writers (truncate to user buffer length).

/// Write an `i32` socket option value.
///
/// Matches Linux `do_ipv6_getsockopt`, `sk_getsockopt`, and `tcp_getsockopt`:
/// copy `min(user_len, sizeof(int))` bytes without the IPv4-only 1-byte shortcut.
#[inline]
pub fn write_i32_getsockopt(value: &mut [u8], v: i32) -> usize {
    if value.is_empty() {
        return 0;
    }
    let len = core::cmp::min(value.len(), core::mem::size_of::<i32>());
    value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
    len
}

/// Write an `i32` SOL_IP option value.
///
/// Matches Linux `do_ip_getsockopt` `copyval`: when `0 < len < 4` and `0 <= v <= 255`,
/// only one byte is copied and returned.
#[inline]
pub fn write_i32_getsockopt_ipv4(value: &mut [u8], v: i32) -> usize {
    if value.is_empty() {
        return 0;
    }
    if value.len() < core::mem::size_of::<i32>() && (0..=255).contains(&v) {
        value[0] = v as u8;
        return 1;
    }
    write_i32_getsockopt(value, v)
}

/// Write a `u32` socket option value (`min(len, 4)` bytes).
#[inline]
pub fn write_u32_getsockopt(value: &mut [u8], v: u32) -> usize {
    if value.is_empty() {
        return 0;
    }
    let len = core::cmp::min(value.len(), core::mem::size_of::<u32>());
    value[..len].copy_from_slice(&v.to_ne_bytes()[..len]);
    len
}

/// Write a `struct linger` payload (`min(len, 8)` bytes).
#[inline]
pub fn write_linger_getsockopt(value: &mut [u8], onoff: i32, linger: i32) -> usize {
    if value.is_empty() {
        return 0;
    }
    let mut buf = [0u8; 8];
    buf[0..4].copy_from_slice(&onoff.to_ne_bytes());
    buf[4..8].copy_from_slice(&linger.to_ne_bytes());
    let len = core::cmp::min(value.len(), buf.len());
    value[..len].copy_from_slice(&buf[..len]);
    len
}
