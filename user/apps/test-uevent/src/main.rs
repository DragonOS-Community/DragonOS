use std::io::{self, Write};
use nix::sys::socket::{bind, socket, AddressFamily, SockAddr, SockFlag, SockProtocol, SockType};
use nix::errno::Errno;
use std::os::unix::io::RawFd;
fn create_netlink_socket() -> Result<RawFd, Errno> {
    socket(
        AddressFamily::Netlink,
        SockType::Raw,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkKObjectUEvent,
    )
}
fn bind_netlink_socket(sock: RawFd) -> Result<(), Errno> {
    let pid = nix::unistd::getpid(); // 获取当前进程 PID
    let addr = SockAddr::new_netlink(pid.as_raw() as u32, 0);
    bind(sock, &addr)
}
fn main() {
    // 创建一个 Netlink 套接字
    let socket = create_netlink_socket().expect("Failed to create Netlink socket");
    println!("Netlink socket created successfully");
    // 绑定套接字
    bind_netlink_socket(socket).expect("Failed to bind Netlink socket");
    println!("Netlink socket created and bound successfully");
}