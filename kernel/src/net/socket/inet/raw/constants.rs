//! Raw socket 相关常量
//!
//! 目标：集中管理 raw socket 的缓冲区/容量类常量，避免分散在多个文件里。

/// smoltcp raw socket 元数据队列默认容量（条目数）。
pub const DEFAULT_METADATA_BUF_SIZE: usize = 64;

/// smoltcp raw socket RX 缓冲区默认大小（字节）。
pub const DEFAULT_RX_BUF_SIZE: usize = 64 * 1024;

/// smoltcp raw socket TX 缓冲区默认大小（字节）。
pub const DEFAULT_TX_BUF_SIZE: usize = 64 * 1024;

// Linux 6.6 默认 sysctl_wmem_max/sysctl_rmem_max 常见值。
// gVisor raw_socket_test 会通过 setsockopt(0xffffffff) 探测 max，并要求可变。
pub const SYSCTL_WMEM_MAX: u32 = 212_992;
pub const SYSCTL_RMEM_MAX: u32 = 212_992;

// 参考 Linux 6.6: SOCK_MIN_RCVBUF/TCP_SKB_MIN_TRUESIZE 约为 2048+skb 头部对齐。
pub const SOCK_MIN_RCVBUF: u32 = 2_304;
pub const SOCK_MIN_SNDBUF: u32 = 4_608;
