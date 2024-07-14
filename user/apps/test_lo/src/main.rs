use icmp;
use std::net::{IpAddr, Ipv4Addr};

fn main() {
    let localhost_v4 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let ping = icmp::IcmpSocket::connect(localhost_v4);
    let mut ping = ping.unwrap();

    let payload: &[u8] = &[1, 2];

    let result = ping.send(payload);
    assert_eq!(result.unwrap(), 2);

    let mut buffer = [0; 1024]; // 创建一个缓冲区来存储响应数据
    let recv_result = ping.recv(&mut buffer); // 接收响应数据

    match recv_result {
        Ok(size) => {
            println!("Received {} bytes: {:?}", size, &buffer[..size]);
        }
        Err(e) => {
            println!("Failed to receive response: {}", e);
        }
    }
}
