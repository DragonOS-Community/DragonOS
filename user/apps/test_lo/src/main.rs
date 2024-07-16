use std::net::UdpSocket;
use std::thread;
use std::thread::sleep;
use std::time::Duration;

fn main() {
    // 在主线程中发送数据
    let socket = UdpSocket::bind("127.0.0.1:8083").expect("could not bind to address");
    socket
        .send_to(&[1; 10], "127.0.0.1:8082")
        .expect("couldn't send data");
}
