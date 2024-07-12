use icmp;
use std::net::{IpAddr, Ipv4Addr};

fn main() {
    let localhost_v4 = IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1));
    let ping = icmp::IcmpSocket::connect(localhost_v4);
    let mut ping = ping.unwrap();

    let payload: &[u8] = &[1, 2];

    let result = ping.send(payload);
    match result {
        Ok(bytes_sent) => {
            if bytes_sent == 2 {
                println!("Ping successful, sent {} bytes", bytes_sent);
            } else {
                println!(
                    "Ping successful, but sent unexpected number of bytes: {}",
                    bytes_sent
                );
            }
        }
        Err(e) => println!("Ping failed: {}", e),
    }
}
