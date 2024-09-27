use std::net::UdpSocket;
use std::str;

fn main() -> std::io::Result<()> {
    let socket = UdpSocket::bind("127.0.0.1:34254")?;
    socket.connect("127.0.0.1:34255")?;

    let listener = UdpSocket::bind("127.0.0.1:34255")?;

    let msg = "Hello, loopback!";
    socket.send(msg.as_bytes())?;

    let mut buf = [0; 1024];
    let (amt, src) = listener.recv_from(&mut buf)?;

    let received_msg = str::from_utf8(&buf[..amt]).expect("Could not read buffer as UTF-8");

    println!("{:?}",  src);

    listener.send_to(b"Hello, DragonOS", src)?;

    println!("Received: {}", received_msg);

    Ok(())
}

// use std::io::prelude::*;
// use std::net::TcpListener;
// use std::net::TcpStream;

// fn main() -> std::io::Result<()> {
//     let listener = TcpListener::bind("0.0.0.0:12580")?;

//     for stream in listener.incoming() {
//         handle_client(stream?);
//     }

//     Ok(())
// }

// fn handle_client(mut stream: TcpStream) {
//     let mut buffer = [0; 512];
//     stream.read(&mut buffer).unwrap();

//     println!("Recv message: {}", String::from_utf8_lossy(&buffer[..]));
//     // 回复一条消息
//     let response = "Hello I am DragonOS!";
//     stream.write(response.as_bytes()).unwrap();
//     stream.flush().unwrap();
// }
