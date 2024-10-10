use std::net::UdpSocket;
use std::str;

fn main() -> std::io::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:34254")?;
    socket.connect("127.0.0.1:34254")?;

    let msg = "Hello, loopback!";
    socket.send(msg.as_bytes())?;

    let mut buf = [0; 1024];
    let (amt, _src) = socket.recv_from(&mut buf)?;

    let received_msg = str::from_utf8(&buf[..amt]).expect("Could not read buffer as UTF-8");

    println!("Sent: {}", msg);
    println!("Received: {}", received_msg);

    assert_eq!(
        msg, received_msg,
        "The sent and received messages do not match!"
    );

    Ok(())
}
