use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::str::from_utf8;
use std::thread;

const SOCKET_PATH: &str = "/tmp/socket_test_file";
const MSG: &str = "Hello, Unix domain socket!";

fn client() -> std::io::Result<()> {
    // 连接到服务器
    let mut stream = UnixStream::connect(SOCKET_PATH)?;
    // 发送消息到服务器
    stream.write_all(MSG.as_bytes())?;
    // 接收服务器的响应
    let mut buffer = [0; 1024];
    let nbytes = stream.read(&mut buffer)?;
    let received_msg = from_utf8(&buffer[..nbytes]).unwrap();
    // 打印接收到的消息
    println!("client received: {}", received_msg);
    Ok(())
}

fn main() -> std::io::Result<()> {
    println!("SOCKET_PATH: {}", SOCKET_PATH);
    println!("MSG: {}", MSG);

    // 删除可能已存在的socket文件
    std::fs::remove_file(&SOCKET_PATH).ok();

    // 创建Unix域监听socket
    let listener = UnixListener::bind(SOCKET_PATH)?;

    let client_thread = thread::spawn(move || client());

    // 监听并接受连接
    let (mut stream, _) = listener.accept().expect("listen error");
    let mut buffer = [0; 1024];
    let nbytes = stream.read(&mut buffer).expect("read error");
    let received_msg = from_utf8(&buffer[..nbytes]).unwrap();
    println!("server received: {}", received_msg);
    stream.write_all(received_msg.as_bytes())?;

    client_thread.join().ok();

    if received_msg == MSG {
        println!("TEST success");
    }

    std::fs::remove_file(&SOCKET_PATH).ok();

    Ok(())
}
