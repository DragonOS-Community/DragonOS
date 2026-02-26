mod base;
pub mod common;
pub mod endpoint;
mod family;
pub mod inet;
mod inode;
pub mod netlink;
pub mod packet;
mod posix;
pub mod unix;
pub mod vsock;
mod utils;

/// `recvfrom(2)` 源地址输出参数的语义。
///
/// Linux 语义要点：
/// - 对于面向连接的 stream socket（如 TCP），`recvfrom` 的 addr/addrlen 是可选输出参数，
///   且通常会被忽略（gVisor 用例要求不要改写用户提供的 sockaddr 缓冲区；若提供了 addrlen，应写回 0）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvFromAddrBehavior {
    /// 正常写回源地址（仅当用户提供 addr + addrlen 时）。
    Write,
    /// 忽略源地址输出；若提供了 addrlen，则写回 0；不得写 addr。
    Ignore,
}

pub use base::Socket;

pub use family::AddressFamily;
pub use posix::IpOption;
pub use posix::IFNAMSIZ;
pub use posix::PIPV6;
pub use posix::PMSG;
pub use posix::PRAW;
pub use posix::PSO;
pub use posix::PSOCK;
pub use posix::PSOL;
pub use utils::create_socket;
