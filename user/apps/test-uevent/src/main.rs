use netlink_sys::{protocols::NETLINK_GENERIC, Socket, SocketAddr};
use std::os::unix::io::AsRawFd;
use nix::sys::socket::{recv, send, MsgFlags};

fn main() {
    // 创建netlink socket
    let mut socket = Socket::new(NETLINK_GENERIC).expect("Failed to create netlink socket");

    // 绑定到内核
    let addr = SocketAddr::new(0, 0);
    socket.bind(&addr).expect("Failed to bind socket");

    // 准备要发送的数据
    let msg = b"Hello from user space!";
    
    // 发送数据到内核
    send(socket.as_raw_fd(), msg, MsgFlags::empty()).expect("Failed to send message");

    // 接收来自内核的数据
    let mut buf = vec![0; 4096];
    let size = recv(socket.as_raw_fd(), &mut buf, MsgFlags::empty()).expect("Failed to receive message");

    // 输出接收到的数据
    println!("Received message: {}", String::from_utf8_lossy(&buf[..size]));
}
