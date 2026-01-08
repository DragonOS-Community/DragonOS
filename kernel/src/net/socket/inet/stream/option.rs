//! TCP socket option handling.
//!
//! This module contains `TcpSocket` methods for setting socket options
//! at SOL_SOCKET, SOL_TCP, and SOL_IP levels.

use num_traits::{FromPrimitive, ToPrimitive};
use smoltcp;
use system_error::SystemError;

use super::constants;
use super::inner;

use crate::libs::byte_parser;
use crate::net::socket::{common::ShutdownBit, IpOption, PSO};
use crate::time::Duration;

/// Linux UAPI-compatible `struct tcp_info` (see Linux 6.6 `include/uapi/linux/tcp.h`).
///
/// Notes:
/// - This struct is used by `getsockopt(SOL_TCP, TCP_INFO, ...)`.
/// - The layout must match Linux userspace ABI, so we keep `#[repr(C)]` and
///   fixed-width integer types.
/// - Linux uses bitfields for wscale/app_limited/fastopen_client_fail; we store
///   them as raw bytes (same memory layout).
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct PosixTcpInfo {
    tcpi_state: u8,
    tcpi_ca_state: u8,
    tcpi_retransmits: u8,
    tcpi_probes: u8,
    tcpi_backoff: u8,
    tcpi_options: u8,
    tcpi_snd_rcv_wscale: u8, // 4 bits snd_wscale + 4 bits rcv_wscale
    tcpi_delivery_rate_app_limited_fastopen_client_fail: u8, // 1 bit + 2 bits

    tcpi_rto: u32,
    tcpi_ato: u32,
    tcpi_snd_mss: u32,
    tcpi_rcv_mss: u32,

    tcpi_unacked: u32,
    tcpi_sacked: u32,
    tcpi_lost: u32,
    tcpi_retrans: u32,
    tcpi_fackets: u32,

    tcpi_last_data_sent: u32,
    tcpi_last_ack_sent: u32,
    tcpi_last_data_recv: u32,
    tcpi_last_ack_recv: u32,

    tcpi_pmtu: u32,
    tcpi_rcv_ssthresh: u32,
    tcpi_rtt: u32,
    tcpi_rttvar: u32,
    tcpi_snd_ssthresh: u32,
    tcpi_snd_cwnd: u32,
    tcpi_advmss: u32,
    tcpi_reordering: u32,

    tcpi_rcv_rtt: u32,
    tcpi_rcv_space: u32,

    tcpi_total_retrans: u32,

    tcpi_pacing_rate: u64,
    tcpi_max_pacing_rate: u64,
    tcpi_bytes_acked: u64,
    tcpi_bytes_received: u64,
    tcpi_segs_out: u32,
    tcpi_segs_in: u32,

    tcpi_notsent_bytes: u32,
    tcpi_min_rtt: u32,
    tcpi_data_segs_in: u32,
    tcpi_data_segs_out: u32,

    tcpi_delivery_rate: u64,

    tcpi_busy_time: u64,
    tcpi_rwnd_limited: u64,
    tcpi_sndbuf_limited: u64,

    tcpi_delivered: u32,
    tcpi_delivered_ce: u32,

    tcpi_bytes_sent: u64,
    tcpi_bytes_retrans: u64,
    tcpi_dsack_dups: u32,
    tcpi_reord_seen: u32,

    tcpi_rcv_ooopack: u32,

    tcpi_snd_wnd: u32,
    tcpi_rcv_wnd: u32,

    tcpi_rehash: u32,
}

impl PosixTcpInfo {
    /// Writes the prefix of `TcpInfo` into `value`, but returns the full size as "need",
    /// matching Linux `getsockopt(TCP_INFO)` semantics (`len = min(user_len, sizeof(info))`).
    fn write_to_optbuf(info: &PosixTcpInfo, value: &mut [u8]) -> Result<usize, SystemError> {
        let need = core::mem::size_of::<PosixTcpInfo>();
        let bytes = unsafe {
            core::slice::from_raw_parts((info as *const PosixTcpInfo) as *const u8, need)
        };
        let copy_len = core::cmp::min(value.len(), bytes.len());
        value[..copy_len].copy_from_slice(&bytes[..copy_len]);
        Ok(need)
    }

    /// Create a PosixTcpInfo from the inner socket state.
    #[inline(never)]
    fn from_inner(inner: &inner::Inner) -> Self {
        match inner {
            inner::Inner::Closed(_) => PosixTcpInfo {
                tcpi_state: constants::PosixTcpState::Close.to_u8().unwrap_or(0),
                ..Default::default()
            },
            inner::Inner::SelfConnected(_) => PosixTcpInfo {
                tcpi_state: constants::PosixTcpState::Established.to_u8().unwrap_or(1),
                tcpi_ca_state: constants::PosixTcpCaState::Open.to_u8().unwrap_or(0),
                tcpi_rto: 200_000,
                tcpi_snd_cwnd: 10,
                ..Default::default()
            },
            _ => inner.with_socket(|socket| {
                let mss = socket.remote_mss();
                PosixTcpInfo {
                    tcpi_state: constants::PosixTcpState::from(socket.state())
                        .to_u8()
                        .unwrap_or(0),
                    tcpi_ca_state: constants::PosixTcpCaState::Open.to_u8().unwrap_or(0),
                    tcpi_rto: socket.rto().total_micros() as u32,
                    tcpi_rtt: socket.rtt() * 1000,        // ms to us
                    tcpi_rttvar: socket.rtt_var() * 1000, // ms to us
                    tcpi_snd_mss: mss as u32,
                    tcpi_rcv_mss: constants::DEFAULT_TCP_MSS as u32,
                    tcpi_snd_cwnd: if mss > 0 {
                        (socket.cwnd().saturating_add(mss - 1) / mss) as u32
                    } else {
                        socket.cwnd() as u32
                    },
                    tcpi_snd_ssthresh: if mss > 0 {
                        (socket.ssthresh() / mss) as u32
                    } else {
                        socket.ssthresh() as u32
                    },
                    tcpi_snd_wnd: socket.remote_win_len() as u32,
                    tcpi_unacked: socket
                        .remote_last_ack()
                        .map(|last_ack| {
                            let diff = socket.local_seq_no().0.wrapping_sub(last_ack.0);
                            if diff < 0 {
                                0
                            } else {
                                diff as u32
                            }
                        })
                        .unwrap_or(0),
                    tcpi_retrans: socket.retransmits() as u32,
                    tcpi_total_retrans: socket.retransmits() as u32,
                    ..Default::default()
                }
            }),
        }
    }
}

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
                    let keepidle = self
                        .tcp_keepidle()
                        .load(core::sync::atomic::Ordering::Relaxed);
                    // Ensure keepidle is at least 1 second to avoid issues, though smoltcp might handle it.
                    // Linux default is 7200.
                    Some(smoltcp::time::Duration::from_secs(keepidle as u64))
                } else {
                    None
                };
                self.apply_keepalive(interval);
                Ok(())
            }
            PSO::LINGER => {
                // struct linger { int l_onoff; int l_linger; }
                if val.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                let l_onoff = byte_parser::read_i32(&val[0..4])?;
                let l_linger = byte_parser::read_i32(&val[4..8])?;

                self.so_linger_active()
                    .store(l_onoff != 0, core::sync::atomic::Ordering::Relaxed);
                self.so_linger_seconds()
                    .store(l_linger, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            PSO::OOBINLINE => {
                let on = byte_parser::read_bool_flag(val)?;
                self.so_oobinline()
                    .store(on, core::sync::atomic::Ordering::Relaxed);
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
                let v = byte_parser::read_i32(val)?;
                if v <= 0 || v > constants::MAX_TCP_KEEPINTVL {
                    return Err(SystemError::EINVAL);
                }
                self.tcp_keepintvl()
                    .store(v, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::KeepCnt => {
                let v = byte_parser::read_i32(val)?;
                if v <= 0 || v > constants::MAX_TCP_KEEPCNT {
                    return Err(SystemError::EINVAL);
                }
                self.tcp_keepcnt()
                    .store(v, core::sync::atomic::Ordering::Relaxed);
                Ok(())
            }
            Options::KeepIdle => {
                let v = byte_parser::read_i32(val)?;
                if v <= 0 || v > constants::MAX_TCP_KEEPIDLE {
                    return Err(SystemError::EINVAL);
                }
                self.tcp_keepidle()
                    .store(v, core::sync::atomic::Ordering::Relaxed);

                // If keepalive is enabled, update the socket with new idle time
                let is_enabled = self
                    .so_keepalive_enabled()
                    .load(core::sync::atomic::Ordering::Relaxed);
                if is_enabled {
                    self.apply_keepalive(Some(smoltcp::time::Duration::from_secs(v as u64)));
                }
                Ok(())
            }
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
            Options::Linger2 => {
                let mut v = byte_parser::read_i32(val)?;

                if v < 0 {
                    v = -1;
                } else if v == 0 {
                    v = constants::DEFAULT_TCP_LINGER2;
                } else if v > constants::MAX_TCP_LINGER2 {
                    v = constants::MAX_TCP_LINGER2;
                }
                self.tcp_linger2()
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
            PSO::LINGER => {
                let l_onoff: i32 = if self
                    .so_linger_active()
                    .load(core::sync::atomic::Ordering::Relaxed)
                {
                    1
                } else {
                    0
                };
                let l_linger = self
                    .so_linger_seconds()
                    .load(core::sync::atomic::Ordering::Relaxed);

                if value.len() < 8 {
                    return Err(SystemError::EINVAL);
                }
                Self::write_i32_opt(&mut value[0..4], l_onoff)?;
                Self::write_i32_opt(&mut value[4..8], l_linger)?;
                Ok(8)
            }
            PSO::OOBINLINE => {
                let v: i32 = if self
                    .so_oobinline()
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
            Options::KeepIdle => Self::write_atomic_i32(value, self.tcp_keepidle()),
            Options::KeepIntvl => Self::write_atomic_i32(value, self.tcp_keepintvl()),
            Options::KeepCnt => Self::write_atomic_i32(value, self.tcp_keepcnt()),
            Options::MaxSegment => Self::write_atomic_usize_as_u32(value, self.tcp_max_seg()),
            Options::DeferAccept => Self::write_atomic_i32(value, self.tcp_defer_accept()),
            Options::Syncnt => Self::write_atomic_i32(value, self.tcp_syncnt()),
            Options::Linger2 => Self::write_atomic_i32(value, self.tcp_linger2()),
            Options::WindowClamp => Self::write_atomic_usize_as_u32(value, self.tcp_window_clamp()),
            Options::UserTimeout => Self::write_atomic_i32(value, self.tcp_user_timeout()),
            Options::Info => {
                let inner_guard = self.inner.read();
                let inner = inner_guard.as_ref().ok_or(SystemError::ENOTCONN)?;

                let info = PosixTcpInfo::from_inner(inner);

                PosixTcpInfo::write_to_optbuf(&info, value)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }
}
