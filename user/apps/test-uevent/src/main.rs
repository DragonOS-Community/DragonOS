use nix::sys::socket::{bind, recvmsg, sendmsg, sendto, socket, AddressFamily, MsgFlags, RecvMsg, SockAddr, SockFlag, SockProtocol, SockType};
use nix::unistd::getpid;
use nix::errno::Errno;
use std::io::{IoSlice, IoSliceMut};
use std::os::unix::io::RawFd;

fn create_netlink_socket() -> Result<RawFd, Errno> {
    socket(
        AddressFamily::Netlink,
        SockType::Datagram,
        SockFlag::SOCK_CLOEXEC,
        SockProtocol::NetlinkKObjectUEvent,
    )
}
fn bind_netlink_socket(sock: RawFd) -> Result<(), Errno> {
    let pid = nix::unistd::getpid(); // 获取当前进程 PID
    let addr = SockAddr::new_netlink(pid.as_raw() as u32, 0);
    // 打印地址信息
    println!("Netlink socket address: {:?}", addr);
    // 将 SockAddr 转换为 NetlinkAddr
    if let SockAddr::Netlink(netlink_addr) = addr {
        // 打印 NetlinkAddr 信息
        println!("Netlink socket address: {:?}", netlink_addr);
        bind(sock, &netlink_addr)
    } else {
        println!("Failed to create NetlinkAddr.");
        Err(Errno::EINVAL)
    }
}
fn send_uevent(sock: RawFd, message: &str) -> Result<(), Errno> {
    let addr = SockAddr::new_netlink(0, 0); // 发送到内核
    sendto(sock, message.as_bytes(), &addr, MsgFlags::empty()).map(|_| ())
}

fn receive_uevent(sock: RawFd) -> Result<String, Errno> {
    let mut buf = [0u8; 1024];
    let mut iov = [IoSliceMut::new(&mut buf)];
    let msg: RecvMsg<()> = recvmsg(sock, &mut iov, None, MsgFlags::empty())?;
    let len = msg.bytes;
    Ok(String::from_utf8_lossy(&buf[..len]).to_string())
}

fn main() {
    // 创建一个 Netlink 套接字
    let socket = create_netlink_socket().expect("Failed to create Netlink socket");
    println!("Netlink socket created successfully");

    // 绑定套接字
    bind_netlink_socket(socket).expect("Failed to bind Netlink socket");
    println!("Netlink socket created and bound successfully");

    // 发送自定义 uevent 消息
    send_uevent(socket, "add@/devices/virtual/block/loop0").expect("Failed to send uevent message");
    println!("Custom uevent message sent successfully");

    // 接收来自内核的 uevent 消息
    let message = receive_uevent(socket).expect("Failed to receive uevent message");
    println!("Received uevent message: {}", message);
}