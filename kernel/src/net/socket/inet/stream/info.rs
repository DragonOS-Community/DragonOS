//! TCP_INFO socket option implementation.
//!
//! This module provides support for the TCP_INFO socket option, which returns
//! detailed information about a TCP connection, compatible with Linux's `struct tcp_info`.

use core::mem;

use smoltcp::socket::tcp;

/// TCP state enum (aligned with Linux).
///
/// Values match Linux's TCP state constants in include/linux/tcp.h
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpState {
    Established = 1,
    SynSent = 2,
    SynRecv = 3,
    FinWait1 = 4,
    FinWait2 = 5,
    TimeWait = 6,
    Close = 7,
    CloseWait = 8,
    LastAck = 9,
    Listen = 10,
    Closing = 11,
}

impl From<tcp::State> for TcpState {
    fn from(state: tcp::State) -> Self {
        match state {
            tcp::State::Established => TcpState::Established,
            tcp::State::SynSent => TcpState::SynSent,
            tcp::State::SynReceived => TcpState::SynRecv,
            tcp::State::FinWait1 => TcpState::FinWait1,
            tcp::State::FinWait2 => TcpState::FinWait2,
            tcp::State::TimeWait => TcpState::TimeWait,
            tcp::State::CloseWait => TcpState::CloseWait,
            tcp::State::Closing => TcpState::Closing,
            tcp::State::LastAck => TcpState::LastAck,
            tcp::State::Listen => TcpState::Listen,
            tcp::State::Closed => TcpState::Close,
        }
    }
}

/// TCP congestion control state enum (aligned with Linux).
///
/// Values match Linux's tcp_ca_state in include/linux/tcp.h
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // Variants other than Open are reserved for future use
pub enum TcpCaState {
    /// No congestion issues detected
    Open = 0,
    /// Some dupack or SACK detected, but no loss yet
    Disorder = 1,
    /// Congestion window reduced due to ECN
    Cwr = 2,
    /// Fast recovery mode
    Recovery = 3,
    /// Loss recovery mode
    Loss = 4,
}

impl TcpCaState {
    /// Infer CA state from congestion control algorithm and current state.
    /// For simplicity, we return Open when connection is healthy,
    /// and other states would need more detailed tracking.
    pub fn from_congestion_control(_cc: tcp::CongestionControl, _tcp_state: tcp::State) -> Self {
        // smoltcp doesn't expose detailed CA state transitions,
        // so we default to Open (healthy connection).
        // A more complete implementation would track retransmissions,
        // ECN signals, etc.
        TcpCaState::Open
    }
}

/// TCP option flags (aligned with Linux's TCPI_OPT_* constants).
#[derive(Debug, Clone, Copy, Default)]
#[allow(dead_code)] // Some constants are reserved for future use
pub struct TcpOptions(u8);

impl TcpOptions {
    pub const TIMESTAMPS: u8 = 0x01;
    pub const SACK: u8 = 0x02;
    pub const WSCALE: u8 = 0x04;
    #[allow(dead_code)]
    pub const ECN: u8 = 0x08;
    #[allow(dead_code)]
    pub const ECN_SEEN: u8 = 0x10;

    #[inline]
    pub const fn new() -> Self {
        Self(0)
    }

    #[inline]
    #[allow(dead_code)]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits)
    }

    #[inline]
    pub fn bits(&self) -> u8 {
        self.0
    }

    #[inline]
    pub fn insert(&mut self, bits: u8) {
        self.0 |= bits;
    }
}

/// TCP_INFO structure (binary compatible with Linux).
///
/// This structure matches the layout of `struct tcp_info` in Linux's
/// include/uapi/linux/tcp.h. Fields are ordered and sized to match
/// the Linux definition for compatibility.
///
/// Not all fields are fully supported; unsupported fields return 0.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct PosixTcpInfo {
    // Basic state (8 bytes total)
    pub tcpi_state: u8,
    pub tcpi_ca_state: u8,
    pub tcpi_retransmits: u8,
    pub tcpi_probes: u8,
    pub tcpi_backoff: u8,
    pub tcpi_options: u8,
    pub tcpi_snd_wscale: u8,
    pub tcpi_rcv_wscale: u8,

    // Timing and size (16 bytes)
    pub tcpi_rto: u32,
    pub tcpi_ato: u32,
    pub tcpi_snd_mss: u32,
    pub tcpi_rcv_mss: u32,

    // Queue and retransmit stats (20 bytes)
    pub tcpi_unacked: u32,
    pub tcpi_sacked: u32,
    pub tcpi_lost: u32,
    pub tcpi_retrans: u32,
    pub tcpi_fackets: u32,

    // Timestamps (16 bytes)
    pub tcpi_last_data_sent: u32,
    pub tcpi_last_ack_sent: u32,
    pub tcpi_last_data_recv: u32,
    pub tcpi_last_ack_recv: u32,

    // Path and metrics (28 bytes)
    pub tcpi_pmtu: u32,
    pub tcpi_rcv_ssthresh: u32,
    pub tcpi_rtt: u32,
    pub tcpi_rttvar: u32,
    pub tcpi_snd_ssthresh: u32,
    pub tcpi_snd_cwnd: u32,
    pub tcpi_advmss: u32,

    // Receive stats (8 bytes)
    pub tcpi_rcv_rtt: u32,
    pub tcpi_rcv_space: u32,
    pub tcpi_total_retrans: u32,

    // Extended fields (Linux 4.7+, 52 bytes)
    pub tcpi_pacing_rate: u64,
    pub tcpi_max_pacing_rate: u64,
    pub tcpi_bytes_acked: u64,
    pub tcpi_bytes_received: u64,
    pub tcpi_segs_out: u32,
    pub tcpi_segs_in: u32,
    pub tcpi_notsent_bytes: u32,
    pub tcpi_min_rtt: u32,
    pub tcpi_data_segs_in: u32,
    pub tcpi_data_segs_out: u32,
    pub tcpi_delivery_rate: u64,

    // Additional fields (16 bytes)
    pub tcpi_busy_time: u64,
    pub tcpi_rwnd_limited: u64,
    pub tcpi_sndbuf_limited: u64,
    pub tcpi_delivered: u32,
    pub tcpi_delivered_ce: u32,

    _pad: [u8; 8], // Padding for future expansion
}

impl Default for PosixTcpInfo {
    fn default() -> Self {
        Self::new()
    }
}

impl PosixTcpInfo {
    /// Create a zero-initialized PosixTcpInfo.
    pub const fn new() -> Self {
        Self {
            tcpi_state: 0,
            tcpi_ca_state: 0,
            tcpi_retransmits: 0,
            tcpi_probes: 0,
            tcpi_backoff: 0,
            tcpi_options: 0,
            tcpi_snd_wscale: 0,
            tcpi_rcv_wscale: 0,
            tcpi_rto: 0,
            tcpi_ato: 0,
            tcpi_snd_mss: 0,
            tcpi_rcv_mss: 0,
            tcpi_unacked: 0,
            tcpi_sacked: 0,
            tcpi_lost: 0,
            tcpi_retrans: 0,
            tcpi_fackets: 0,
            tcpi_last_data_sent: 0,
            tcpi_last_ack_sent: 0,
            tcpi_last_data_recv: 0,
            tcpi_last_ack_recv: 0,
            tcpi_pmtu: 0,
            tcpi_rcv_ssthresh: 0,
            tcpi_rtt: 0,
            tcpi_rttvar: 0,
            tcpi_snd_ssthresh: 0,
            tcpi_snd_cwnd: 0,
            tcpi_advmss: 0,
            tcpi_rcv_rtt: 0,
            tcpi_rcv_space: 0,
            tcpi_total_retrans: 0,
            tcpi_pacing_rate: 0,
            tcpi_max_pacing_rate: 0,
            tcpi_bytes_acked: 0,
            tcpi_bytes_received: 0,
            tcpi_segs_out: 0,
            tcpi_segs_in: 0,
            tcpi_notsent_bytes: 0,
            tcpi_min_rtt: 0,
            tcpi_data_segs_in: 0,
            tcpi_data_segs_out: 0,
            tcpi_delivery_rate: 0,
            tcpi_busy_time: 0,
            tcpi_rwnd_limited: 0,
            tcpi_sndbuf_limited: 0,
            tcpi_delivered: 0,
            tcpi_delivered_ce: 0,
            _pad: [0; 8],
        }
    }

    /// Get the size of the structure for use with getsockopt.
    #[allow(dead_code)]
    pub fn size() -> usize {
        mem::size_of::<Self>()
    }
}

/// Collector for TCP_INFO statistics from a smoltcp TCP socket.
pub struct TcpInfoCollector<'a> {
    socket: &'a tcp::Socket<'a>,
}

impl<'a> TcpInfoCollector<'a> {
    /// Create a new collector for the given socket.
    pub fn new(socket: &'a tcp::Socket<'a>) -> Self {
        Self { socket }
    }

    /// Collect all available TCP info from the socket.
    pub fn collect(&self) -> PosixTcpInfo {
        let state = self.socket.state();
        let tcp_state = TcpState::from(state);
        let cc = self.socket.congestion_control();

        let mut info = PosixTcpInfo::new();

        // Basic state
        info.tcpi_state = tcp_state as u8;
        info.tcpi_ca_state = TcpCaState::from_congestion_control(cc, state) as u8;
        info.tcpi_retransmits = self.socket.retransmits();

        // Options
        let mut options = TcpOptions::new();
        if self.socket.timestamp_enabled() {
            options.insert(TcpOptions::TIMESTAMPS);
        }
        if self.socket.remote_win_scale().is_some() {
            options.insert(TcpOptions::WSCALE);
        }
        if self.socket.remote_has_sack() {
            options.insert(TcpOptions::SACK);
        }
        info.tcpi_options = options.bits();

        // Window scaling
        info.tcpi_snd_wscale = self.socket.remote_win_shift();
        info.tcpi_rcv_wscale = self.socket.remote_win_scale().unwrap_or(0);

        // Timing (convert to microseconds like Linux)
        // smoltcp's rto() returns Duration, convert to microseconds
        info.tcpi_rto = self.socket.rto().total_micros() as u32;
        // ato (ack timeout) - use ack_delay if available
        info.tcpi_ato = self
            .socket
            .ack_delay()
            .map(|d| d.total_micros() as u32)
            .unwrap_or(0);

        // MSS
        let mss = self.socket.remote_mss() as u32;
        info.tcpi_snd_mss = mss;
        info.tcpi_rcv_mss = mss;

        // Queue stats
        info.tcpi_unacked = (self.socket.send_queue() / mss.max(1) as usize) as u32;
        info.tcpi_retrans = info.tcpi_unacked; // Approximation
        info.tcpi_notsent_bytes = self.socket.send_queue() as u32;

        // RTT (convert milliseconds to microseconds like Linux)
        info.tcpi_rtt = self.socket.rtt() * 1000;
        info.tcpi_rttvar = self.socket.rtt_var() * 1000;
        info.tcpi_min_rtt = info.tcpi_rtt; // smoltcp doesn't track min separately

        // Congestion
        info.tcpi_snd_cwnd = self.socket.cwnd() as u32;
        info.tcpi_snd_ssthresh = self.socket.ssthresh() as u32;
        info.tcpi_advmss = mss;

        // Receive space
        info.tcpi_rcv_space = self.socket.recv_capacity() as u32;

        // Total retransmits (same as current for smoltcp)
        info.tcpi_total_retrans = info.tcpi_retransmits as u32;

        // Unsupported fields remain 0
        // tcpi_probes, tcpi_backoff, tcpi_sacked, tcpi_lost, tcpi_fackets
        // tcpi_last_data_sent, tcpi_last_ack_sent, tcpi_last_data_recv, tcpi_last_ack_recv
        // tcpi_pmtu, tcpi_rcv_ssthresh, tcpi_rcv_rtt
        // tcpi_pacing_rate, tcpi_max_pacing_rate, tcpi_delivery_rate
        // tcpi_bytes_acked, tcpi_bytes_received
        // tcpi_segs_out, tcpi_segs_in, tcpi_data_segs_in, tcpi_data_segs_out
        // tcpi_busy_time, tcpi_rwnd_limited, tcpi_sndbuf_limited
        // tcpi_delivered, tcpi_delivered_ce

        info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tcp_state_values() {
        // Ensure values match Linux constants
        assert_eq!(TcpState::Established as u8, 1);
        assert_eq!(TcpState::SynSent as u8, 2);
        assert_eq!(TcpState::SynRecv as u8, 3);
        assert_eq!(TcpState::Listen as u8, 10);
    }

    #[test]
    fn test_tcp_info_size() {
        // The structure should have a specific size for compatibility
        let size = mem::size_of::<PosixTcpInfo>();
        // Linux 6.6 tcp_info is 144 bytes without padding, we add more
        assert!(
            size >= 144,
            "PosixTcpInfo size should be at least 144 bytes"
        );
    }

    #[test]
    fn test_tcp_options() {
        let mut opts = TcpOptions::new();
        assert_eq!(opts.bits(), 0);
        opts.insert(TcpOptions::TIMESTAMPS);
        assert_eq!(opts.bits(), 1);
        opts.insert(TcpOptions::WSCALE);
        assert_eq!(opts.bits(), 5);
    }
}
