use system_error::SystemError;

// Linux 6.6 defaults for net.core.{r,w}mem_max.
pub const SYSCTL_WMEM_MAX: u32 = 212_992;
pub const SYSCTL_RMEM_MAX: u32 = 212_992;

// Linux 6.6 include/net/sock.h socket-buffer lower bounds.
pub const SOCK_MIN_RCVBUF: u32 = 2_304;
pub const SOCK_MIN_SNDBUF: u32 = 4_608;

/// Parse a native integer socket-buffer hint and return the Linux-visible
/// effective size. Linux treats the input as an unsigned hint, clamps it to
/// the sysctl maximum, doubles it for accounting overhead, and applies the
/// per-direction minimum.
pub fn parse_socket_buffer_size(
    value: &[u8],
    sysctl_max: u32,
    minimum: u32,
) -> Result<usize, SystemError> {
    if value.len() < core::mem::size_of::<u32>() {
        return Err(SystemError::EINVAL);
    }
    let requested = u32::from_ne_bytes(value[..4].try_into().unwrap());
    let requested = requested.min(sysctl_max).min((i32::MAX as u32) / 2);
    Ok(requested.saturating_mul(2).max(minimum) as usize)
}
