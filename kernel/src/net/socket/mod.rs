mod base;
pub(crate) mod buffer;
pub mod common;
pub mod endpoint;
mod family;
pub mod inet;
mod inode;
mod ioctl;
pub mod netlink;
pub mod packet;
mod posix;
pub(crate) mod recv_buffer;
pub(crate) mod recv_timeout;
pub mod unix;
mod utils;
pub mod vsock;

/// `recvfrom(2)` 源地址输出参数的语义。
///
/// Linux 语义要点：
/// - addr 为 NULL 时，完全忽略 addrlen；否则在接收成功后执行地址输出长度协议。
/// - 普通 TCP/VSOCK 不产生源地址，不访问 sockaddr 内容，但仍校验并写回长度 0。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RecvFromAddrBehavior {
    /// 产生源地址，按用户容量截断复制并写回完整长度。
    Write,
    /// 产生零长度地址结果；不写 addr 内容，仍执行共用的长度校验与写回。
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
