//! TCP socket option handling.
//!
//! This module contains `TcpSocket` methods for setting socket options
//! at SOL_SOCKET, SOL_TCP, and SOL_IP levels.

use num_traits::{FromPrimitive, ToPrimitive};
use system_error::SystemError;

use super::constants;
use super::info;
use super::inner;

use crate::libs::byte_parser;
use crate::net::socket::{common::ShutdownBit, IpOption, PSO};
use crate::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, FromPrimitive, ToPrimitive)]
pub enum Options {
    /// Turn off Nagle's algorithm.
    NoDelay = 1,
    /// Limit MSS.
    MaxSegment = 2,
    /// Never send partially complete segments.
    Cork = 3,
    /// Start keeplives after this period.
    KeepIdle = 4,
    /// Interval between keepalives.
    KeepIntvl = 5,
    /// Number of keepalives before death.
    KeepCnt = 6,
    /// Number of SYN retransmits.
    Syncnt = 7,
    /// Lifetime for orphaned FIN-WAIT-2 state.
    Linger2 = 8,
    /// Wake up listener only when data arrive.
    DeferAccept = 9,
    /// Bound advertised window
    WindowClamp = 10,
    /// Information about this connection.
    Info = 11,
    /// Block/reenable quick acks.
    QuickAck = 12,
    /// Congestion control algorithm.
    Congestion = 13,
    /// TCP MD5 Signature (RFC2385).
    Md5Sig = 14,
    /// Use linear timeouts for thin streams
    ThinLinearTimeouts = 16,
    /// Fast retrans. after 1 dupack.
    ThinDupack = 17,
    /// How long for loss retry before timeout.
    UserTimeout = 18,
    /// TCP sock is under repair right now.
    Repair = 19,
    RepairQueue = 20,
    QueueSeq = 21,
    #[allow(clippy::enum_variant_names)]
    RepairOptions = 22,
    /// Enable FastOpen on listeners
    FastOpen = 23,
    Timestamp = 24,
    /// Limit number of unsent bytes in write queue.
    NotSentLowat = 25,
    /// Get Congestion Control (optional) info.
    CCInfo = 26,
    /// Record SYN headers for new connections.
    SaveSyn = 27,
    /// Get SYN headers recorded for connection.
    SavedSyn = 28,
    /// Get/set window parameters.
    RepairWindow = 29,
    /// Attempt FastOpen with connect.
    FastOpenConnect = 30,
    /// Attach a ULP to a TCP connection.
    ULP = 31,
    /// TCP MD5 Signature with extensions.
    Md5SigExt = 32,
    /// Set the key for Fast Open(cookie).
    FastOpenKey = 33,
    /// Enable TFO without a TFO cookie.
    FastOpenNoCookie = 34,
    ZeroCopyReceive = 35,
    /// Notify bytes available to read as a cmsg on read.
    INQ = 36,
    /// delay outgoing packets by XX usec
    TxDelay = 37,
}

impl TryFrom<i32> for Options {
    type Error = system_error::SystemError;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        match <Self as FromPrimitive>::from_i32(value) {
            Some(p) => Ok(p),
            None => Err(Self::Error::EINVAL),
        }
    }
}

impl From<Options> for i32 {
    fn from(val: Options) -> Self {
        <Options as ToPrimitive>::to_i32(&val).unwrap()
    }
}

/// TCP socket option setters.
impl super::TcpSocket {
    /// Helper to write a u32 value to an option buffer.
    #[inline]
    fn write_u32_opt(value: &mut [u8], v: u32) -> Result<usize, SystemError> {
        if value.len() < 4 {
            return Err(SystemError::EINVAL);
        }
        value[..4].copy_from_slice(&v.to_ne_bytes());
        Ok(4)
    }

    /// Helper to write an i32 value to an option buffer.
    #[inline]
    fn write_i32_opt(value: &mut [u8], v: i32) -> Result<usize, SystemError> {
        Self::write_u32_opt(value, v as u32)
    }

    /// Helper to read an atomic usize value and write as u32 to an option buffer.
    #[inline]
    fn write_atomic_usize_as_u32(
        value: &mut [u8],
        atomic: &core::sync::atomic::AtomicUsize,
    ) -> Result<usize, SystemError> {
        let v = atomic.load(core::sync::atomic::Ordering::Relaxed);
        Self::write_u32_opt(value, v as u32)
    }

    /// Helper to read an atomic i32 value and write to an option buffer.
    #[inline]
    fn write_atomic_i32(
        value: &mut [u8],
        atomic: &core::sync::atomic::AtomicI32,
    ) -> Result<usize, SystemError> {
        let v = atomic.load(core::sync::atomic::Ordering::Relaxed);
        Self::write_i32_opt(value, v)
    }

    /// Helper to get a socket property with a default for closed/self-connected/none states.
    ///
    /// This pattern is repeated when getting socket options that need to handle
    /// cases where the socket is closed, self-connected, or not yet initialized.
    #[inline]
    fn with_socket_property<F, R>(&self, default: R, f: F) -> R
    where
        F: FnOnce(&inner::Inner) -> R,
    {
        match self.inner.read().as_ref() {
            Some(inner::Inner::Closed(_)) | Some(inner::Inner::SelfConnected(_)) | None => default,
            Some(inner) => f(inner),
        }
    }

    #[inline]
    fn effective_sockbuf_bytes(requested: usize) -> usize {
        requested
            .saturating_mul(2)
            .clamp(constants::SOCK_MIN_BUFFER, constants::MAX_SOCKET_BUFFER)
    }

    /// Parses a timeval value for socket timeout options.
    pub(super) fn parse_timeval_opt(optval: &[u8]) -> Result<Duration, SystemError> {
        // Accept both 64-bit and 32-bit timeval layouts.
        if optval.len() >= 16 {
            let mut sec_raw = [0u8; 8];
            let mut usec_raw = [0u8; 8];
            sec_raw.copy_from_slice(&optval[..8]);
            usec_raw.copy_from_slice(&optval[8..16]);
            let sec = i64::from_ne_bytes(sec_raw);
            let usec = i64::from_ne_bytes(usec_raw);
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(SystemError::EINVAL);
            }
            let total_us = (sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(usec as u64);
            return Ok(Duration::from_micros(total_us));
        }

        if optval.len() >= 12 {
            let mut sec_raw = [0u8; 8];
            let mut usec_raw = [0u8; 4];
            sec_raw.copy_from_slice(&optval[..8]);
            usec_raw.copy_from_slice(&optval[8..12]);
            let sec = i64::from_ne_bytes(sec_raw);
            let usec = i32::from_ne_bytes(usec_raw) as i64;
            if sec < 0 || !(0..1_000_000).contains(&usec) {
                return Err(SystemError::EINVAL);
            }
            let total_us = (sec as u64)
                .saturating_mul(1_000_000)
                .saturating_add(usec as u64);
            return Ok(Duration::from_micros(total_us));
        }

        Err(SystemError::EINVAL)
    }

    /// Sets a SOL_SOCKET option.
    pub(super) fn set_socket_option(&self, opt: PSO, val: &[u8]) -> Result<(), SystemError> {
        match opt {
            PSO::SNDTIMEO_OLD | PSO::SNDTIMEO_NEW => {
                let d = Self::parse_timeval_opt(val)?;
                self.send_timeout_us()
                    .store(d.total_micros(), core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            PSO::RCVTIMEO_OLD | PSO::RCVTIMEO_NEW => {
                let d = Self::parse_timeval_opt(val)?;
                self.recv_timeout_us()
                    .store(d.total_micros(), core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            PSO::TIMESTAMP_OLD | PSO::TIMESTAMP_NEW => {
                let on = byte_parser::read_bool_flag(val)?;
                self.so_timestamp_enabled()
                    .store(on, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            PSO::SNDBUF | PSO::SNDBUFFORCE => {
                let requested = byte_parser::read_u32(val)? as usize;
                let requested =
                    requested.clamp(constants::SOCK_MIN_BUFFER, constants::MAX_SOCKET_BUFFER);
                let size = Self::effective_sockbuf_bytes(requested);
                self.send_buf_size()
                    .store(size, core::sync::atomic::Ordering::Relaxed);
                self.update_inner_buffers(size, self.recv_buf_size_loaded());
                crate::net::socket::base::Socket::wait_queue(self).wakeup(None);
                Ok(())
            }
            PSO::RCVBUF | PSO::RCVBUFFORCE => {
                let requested = byte_parser::read_u32(val)? as usize;
                let requested =
                    requested.clamp(constants::SOCK_MIN_BUFFER, constants::MAX_SOCKET_BUFFER);
                let size = Self::effective_sockbuf_bytes(requested);
                self.recv_buf_size()
                    .store(size, core::sync::atomic::Ordering::Relaxed);
                self.update_inner_buffers(self.send_buf_size_loaded(), size);
                crate::net::socket::base::Socket::wait_queue(self).wakeup(None);
                Ok(())
            }
            PSO::ATTACH_FILTER => {
                self.so_filter_attached()
                    .store(true, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            PSO::DETACH_FILTER => {
                if self
                    .so_filter_attached()
                    .swap(false, core::sync::atomic::Ordering::Relaxed)
                {
                    Ok(())
                } else {
                    Err(SystemError::ENOENT)
                }
            }
            PSO::KEEPALIVE => {
                let on = byte_parser::read_bool_flag(val)?;
                self.so_keepalive_enabled()
                    .store(on, core::sync::atomic::Ordering::Relaxed);
                let interval = if on {
                    Some(smoltcp::time::Duration::from_secs(7200))
                } else {
                    None
                };
                self.apply_keepalive(interval);
                Ok(())
            }
            _ => Ok(()), // Accept and ignore other SOL_SOCKET options
        }
    }

    /// Sets a SOL_IP option.
    pub(super) fn set_ip_option(&self, opt: IpOption, val: &[u8]) -> Result<(), SystemError> {
        match opt {
            IpOption::MTU_DISCOVER => {
                let v = byte_parser::read_i32(val)?;
                self.ip_mtu_discover()
                    .store(v, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            _ => Ok(()), // Ignore unsupported IP options
        }
    }

    /// Sets a SOL_TCP option.
    pub(super) fn set_tcp_option(&self, opt: Options, val: &[u8]) -> Result<(), SystemError> {
        match opt {
            Options::NoDelay => {
                let nagle_enabled = !byte_parser::read_bool_flag(val)?;
                self.with_inner_established(|est| {
                    est.with_mut(|s| s.set_nagle_enabled(nagle_enabled));
                })?;
                Ok(())
            }
            Options::KeepIntvl => {
                let interval = byte_parser::read_u32(val)?;
                self.with_inner_established(|est| {
                    est.with_mut(|socket| {
                        socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(
                            interval as u64,
                        )));
                    });
                })?;
                Ok(())
            }
            Options::KeepCnt | Options::KeepIdle => Ok(()), // Stub: silently ignore
            Options::INQ => {
                let on = byte_parser::read_bool_flag(val)?;
                self.tcp_inq_enabled()
                    .store(on, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::QuickAck => {
                let on = byte_parser::read_bool_flag(val)?;
                self.tcp_quickack_enabled()
                    .store(on, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::Cork => {
                let on = byte_parser::read_bool_flag(val)?;
                self.options
                    .tcp_cork
                    .store(on, core::sync::atomic::Ordering::Relaxed);
                if !on {
                    let _ = self.flush_cork_buffer();
                }
                Ok(())
            }
            Options::Congestion => {
                let s = byte_parser::read_string(val)?;
                let cc = match s {
                    "reno" => smoltcp::socket::tcp::CongestionControl::Reno,
                    "cubic" => smoltcp::socket::tcp::CongestionControl::Cubic,
                    _ => return Err(SystemError::ENOENT),
                };
                self.apply_congestion_control(cc);
                Ok(())
            }
            Options::MaxSegment => {
                let v = byte_parser::read_u32(val)?;
                if !(constants::TCP_MIN_MSS..=constants::MAX_TCP_WINDOW).contains(&v) {
                    return Err(SystemError::EINVAL);
                }
                self.tcp_max_seg()
                    .store(v as usize, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::DeferAccept => {
                let v = byte_parser::read_i32(val)?.max(0);
                self.tcp_defer_accept()
                    .store(v, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::Syncnt => {
                let v = byte_parser::read_i32(val)?;
                if !(1..=127).contains(&v) {
                    return Err(SystemError::EINVAL);
                }
                self.tcp_syncnt()
                    .store(v, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::WindowClamp => {
                let v = byte_parser::read_u32(val)?;
                let v = if v == 0 {
                    let is_closed = matches!(
                        self.inner.read().as_ref(),
                        Some(inner::Inner::Init(_)) | None
                    );
                    if !is_closed {
                        return Err(SystemError::EINVAL);
                    }
                    0
                } else {
                    // 当 TCP_WINDOW_CLAMP < (min SO_RCVBUF)/2 时，应当被提升到 (min SO_RCVBUF)/2。
                    //
                    // 在 Linux 上 SO_RCVBUF 的 getsockopt 返回的是“有效值”(通常为用户请求的 2 倍)，
                    // 最小有效 SO_RCVBUF 对应 4096，因此 (min SO_RCVBUF)/2 == 2048。
                    // DragonOS 这里用 SOCK_MIN_RCVBUF（与 TCP_SKB_MIN_TRUESIZE 对齐）表达该下限。
                    v.max(constants::SOCK_MIN_RCVBUF as u32)
                };
                self.tcp_window_clamp()
                    .store(v as usize, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::UserTimeout => {
                let v = byte_parser::read_i32(val)?;
                if v < 0 {
                    return Err(SystemError::EINVAL);
                }
                self.tcp_user_timeout()
                    .store(v, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            _ => Ok(()), // Silently ignore unsupported TCP options
        }
    }

    /// Gets a SOL_SOCKET option.
    pub(super) fn get_socket_option(
        &self,
        opt: PSO,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match opt {
            PSO::ACCEPTCONN => {
                let shutdown = self.shutdown.load(core::sync::atomic::Ordering::Acquire);
                let is_listening = self.is_listening();
                // shutdown(SHUT_RD) on a listening socket stops it from being a listener (SO_ACCEPTCONN=0).
                let v = if is_listening && (shutdown & ShutdownBit::SHUT_RD.bits() as usize) == 0 {
                    1i32
                } else {
                    0i32
                };
                Self::write_i32_opt(value, v)
            }
            PSO::ERROR => {
                let err = match self.inner.read().as_ref() {
                    Some(inner::Inner::Connecting(c)) => {
                        let err = c.failure_reason().map(|e| -e.to_posix_errno()).unwrap_or(0);
                        if err != 0 {
                            c.consume_error();
                        }
                        err
                    }
                    _ => 0,
                };
                Self::write_i32_opt(value, err)
            }
            PSO::KEEPALIVE => {
                let v: i32 = if self
                    .so_keepalive_enabled()
                    .load(core::sync::atomic::Ordering::Relaxed)
                {
                    1
                } else {
                    0
                };
                Self::write_i32_opt(value, v)
            }
            _ => {
                // Most SOL_SOCKET options are handled by sys_getsockopt directly.
                Err(SystemError::ENOPROTOOPT)
            }
        }
    }

    /// Gets a SOL_IP option.
    pub(super) fn get_ip_option(
        &self,
        opt: IpOption,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match opt {
            IpOption::MTU_DISCOVER => Self::write_atomic_i32(value, self.ip_mtu_discover()),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    /// Gets a SOL_TCP option.
    pub(super) fn get_tcp_option(
        &self,
        opt: Options,
        value: &mut [u8],
    ) -> Result<usize, SystemError> {
        match opt {
            Options::NoDelay => {
                let nagle_enabled = self
                    .with_socket_property(true, |inner| inner.with_socket(|s| s.nagle_enabled()));
                let nodelay: u32 = if nagle_enabled { 0 } else { 1 };
                Self::write_u32_opt(value, nodelay)
            }
            Options::INQ => {
                let v: u32 = if self.inq_enabled() { 1 } else { 0 };
                Self::write_u32_opt(value, v)
            }
            Options::QuickAck => {
                let v: u32 = if self
                    .tcp_quickack_enabled()
                    .load(core::sync::atomic::Ordering::Relaxed)
                {
                    1
                } else {
                    0
                };
                Self::write_u32_opt(value, v)
            }
            Options::Cork => {
                let v: i32 = if self
                    .options
                    .tcp_cork
                    .load(core::sync::atomic::Ordering::Relaxed)
                {
                    1
                } else {
                    0
                };
                Self::write_i32_opt(value, v)
            }
            Options::Congestion => {
                let cc_name = self
                    .with_socket_property(smoltcp::socket::tcp::CongestionControl::Reno, |inner| {
                        inner.with_socket(|s| s.congestion_control())
                    });

                let name = match cc_name {
                    smoltcp::socket::tcp::CongestionControl::Reno => "reno",
                    smoltcp::socket::tcp::CongestionControl::Cubic => "cubic",
                    _ => "reno",
                };

                let name_bytes = name.as_bytes();
                // Linux TCP_CA_NAME_MAX is 16
                let max_len = 16;
                let len = core::cmp::min(value.len(), max_len);

                // Fill with 0
                value[..len].fill(0);
                // Copy name
                let copy_len = core::cmp::min(len, name_bytes.len());
                value[..copy_len].copy_from_slice(&name_bytes[..copy_len]);

                Ok(len)
            }
            Options::MaxSegment => Self::write_atomic_usize_as_u32(value, self.tcp_max_seg()),
            Options::DeferAccept => Self::write_atomic_i32(value, self.tcp_defer_accept()),
            Options::Syncnt => Self::write_atomic_i32(value, self.tcp_syncnt()),
            Options::WindowClamp => Self::write_atomic_usize_as_u32(value, self.tcp_window_clamp()),
            Options::UserTimeout => Self::write_atomic_i32(value, self.tcp_user_timeout()),
            Options::Info => self.get_tcp_info(value),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    /// Get TCP_INFO for this socket.
    fn get_tcp_info(&self, value: &mut [u8]) -> Result<usize, SystemError> {
        use info::TcpInfoCollector;

        // For closed/unconnected sockets, return default info
        let info = self.with_socket_property(info::PosixTcpInfo::default(), |inner| {
            inner.with_socket(|socket| TcpInfoCollector::new(socket).collect())
        });

        // Copy the info struct to the output buffer
        let info_bytes = unsafe {
            core::slice::from_raw_parts(
                &info as *const info::PosixTcpInfo as *const u8,
                core::mem::size_of::<info::PosixTcpInfo>(),
            )
        };

        let len = core::cmp::min(value.len(), info_bytes.len());
        if len > 0 {
            value[..len].copy_from_slice(&info_bytes[..len]);
        }

        Ok(len)
    }
}
