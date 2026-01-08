//! TCP socket常量定义
//!
//! 本模块定义TCP socket相关的常量，参考Linux内核实现：
//! - include/net/tcp.h
//! - include/net/sock.h

// ========== TCP常量 - 参考Linux内核 include/net/tcp.h ==========

/// Minimal accepted MSS. It is (60+60+8) - (20+20).
/// 来自Linux内核: include/net/tcp.h:68
pub const TCP_MIN_MSS: u32 = 88;

/// Never offer a window over 32767 without using window scaling.
/// Some poor stacks do signed 16bit maths!
/// 来自Linux内核: include/net/tcp.h:65
pub const MAX_TCP_WINDOW: u32 = 32767;

/// 默认MSS值 (536 = RFC 879 default MSS for IPv4)
pub const DEFAULT_TCP_MSS: usize = 536;

/// 默认SYN重传次数
pub const DEFAULT_TCP_SYNCNT: i32 = 6;

// Keepalive limits
pub const MAX_TCP_KEEPIDLE: i32 = 32767;
pub const MAX_TCP_KEEPINTVL: i32 = 32767;
pub const MAX_TCP_KEEPCNT: i32 = 127;

// Keepalive defaults (aligned with Linux defaults)
// See Linux kernel: include/net/tcp.h
/// Default time (in seconds) before keepalive probes start (7200s = 2h)
pub const DEFAULT_TCP_KEEPIDLE: i32 = 7200;
/// Default interval (in seconds) between keepalive probes (75s)
pub const DEFAULT_TCP_KEEPINTVL: i32 = 75;
/// Default number of keepalive probes before dropping connection (9)
pub const DEFAULT_TCP_KEEPCNT: i32 = 9;

// Linger2 defaults (aligned with Linux)
// See Linux kernel: include/net/tcp.h
/// Default lifetime (in seconds) for orphaned FIN-WAIT-2 state (60s)
pub const DEFAULT_TCP_LINGER2: i32 = 60;
/// Max lifetime (in seconds) for orphaned FIN-WAIT-2 state (120s)
pub const MAX_TCP_LINGER2: i32 = 120;

// ========== Socket缓冲区常量 - 参考Linux内核 include/net/sock.h ==========

/// 最小socket缓冲区基本单位（用于SO_SNDBUF/SO_RCVBUF的clamp下限）
/// 来自Linux内核: include/net/sock.h:2565 (TCP_SKB_MIN_TRUESIZE)
pub const SOCK_MIN_BUFFER: usize = 2048;

/// Minimum receive buffer size.
/// 来自Linux内核: include/net/sock.h:2565 (TCP_SKB_MIN_TRUESIZE)
pub const SOCK_MIN_RCVBUF: usize = SOCK_MIN_BUFFER;

/// 最大socket缓冲区大小（用于SO_SNDBUF/SO_RCVBUF的clamp上限）
pub const MAX_SOCKET_BUFFER: usize = 10 * 1024 * 1024;

// ========== TCP状态常量 - 参考Linux内核 include/net/tcp_states.h ==========
use num_derive::ToPrimitive;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ToPrimitive)]
pub enum PosixTcpState {
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
    NewSynRecv = 12,
}

impl From<smoltcp::socket::tcp::State> for PosixTcpState {
    fn from(state: smoltcp::socket::tcp::State) -> Self {
        match state {
            smoltcp::socket::tcp::State::Closed => PosixTcpState::Close,
            smoltcp::socket::tcp::State::Listen => PosixTcpState::Listen,
            smoltcp::socket::tcp::State::SynSent => PosixTcpState::SynSent,
            smoltcp::socket::tcp::State::SynReceived => PosixTcpState::SynRecv,
            smoltcp::socket::tcp::State::Established => PosixTcpState::Established,
            smoltcp::socket::tcp::State::FinWait1 => PosixTcpState::FinWait1,
            smoltcp::socket::tcp::State::FinWait2 => PosixTcpState::FinWait2,
            smoltcp::socket::tcp::State::CloseWait => PosixTcpState::CloseWait,
            smoltcp::socket::tcp::State::Closing => PosixTcpState::Closing,
            smoltcp::socket::tcp::State::LastAck => PosixTcpState::LastAck,
            smoltcp::socket::tcp::State::TimeWait => PosixTcpState::TimeWait,
        }
    }
}

// ========== TCP拥塞控制状态 - 参考Linux内核 include/uapi/linux/tcp.h ==========
#[derive(Debug, Clone, Copy, PartialEq, Eq, ToPrimitive)]
pub enum PosixTcpCaState {
    Open = 0,
    Disorder = 1,
    Cwr = 2,
    Recovery = 3,
    Loss = 4,
}
