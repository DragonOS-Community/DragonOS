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

/// TCP_FIN_TIMEOUT 默认值（秒）
/// 来自Linux内核: include/net/tcp.h (TCP_TIMEWAIT_LEN)
pub const TCP_FIN_TIMEOUT_DEFAULT: i32 = 60;

/// TCP_LINGER2 最大值（秒）
/// 来自Linux内核: include/net/tcp.h (TCP_FIN_TIMEOUT_MAX)
pub const TCP_FIN_TIMEOUT_MAX: i32 = 120;

/// TCP_CORK flush timeout (microseconds). Linux uses ~200ms.
pub const TCP_CORK_FLUSH_TIMEOUT_US: u64 = 200_000;

/// Default IPv4 multicast TTL for sockets.
pub const IP_MULTICAST_TTL_DEFAULT: i32 = 1;

/// Default IPv4 multicast loopback behavior (enabled).
pub const IP_MULTICAST_LOOP_DEFAULT: bool = true;

// ========== Socket缓冲区常量 - 参考Linux内核 include/net/sock.h ==========

/// 最小socket缓冲区基本单位（用于SO_SNDBUF/SO_RCVBUF的clamp下限）
/// 来自Linux内核: include/net/sock.h:2565 (TCP_SKB_MIN_TRUESIZE)
pub const SOCK_MIN_BUFFER: usize = 2048;

/// Minimum receive buffer size.
/// 来自Linux内核: include/net/sock.h:2565 (TCP_SKB_MIN_TRUESIZE)
pub const SOCK_MIN_RCVBUF: usize = SOCK_MIN_BUFFER;

/// 最大socket缓冲区大小（用于SO_SNDBUF/SO_RCVBUF的clamp上限）
pub const MAX_SOCKET_BUFFER: usize = 10 * 1024 * 1024;
