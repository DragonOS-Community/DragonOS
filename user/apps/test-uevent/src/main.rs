use netlink_sys::{Socket, protocols::NETLINK_KOBJECT_UEVENT};
use std::io::{self, Write};

fn main() {
    // 创建一个 Netlink 套接字
    let mut socket = Socket::new(NETLINK_KOBJECT_UEVENT).expect("Failed to create netlink socket");

    // 绑定套接字
    socket.bind_auto().expect("Failed to bind socket");

    println!("Listening for uevents...");

    // 接收消息
    let mut buf = vec![0; 4096];
    loop {
        match socket.recv(&mut buf, 0) {
            Ok(size) => {
                // 打印接收到的消息
                io::stdout().write_all(&buf[..size]).expect("Failed to write to stdout");
                println!();
            }
            Err(e) => {
                eprintln!("Failed to receive message: {}", e);
                break;
            }
        }
    }
}