use std::os::unix::net::{UnixListener, UnixStream};
use std::io::{Read, Write};
use std::thread;
use std::time::Duration;

const SOCKET_PATH: &str = "/test.socket";
const MSG: &str = "Hello, unix stream socket!";

fn handle_client(mut stream: UnixStream) {
    let mut buffer = [0; 1024];
    match stream.read(&mut buffer) {
        Ok(size) => {
            println!("Received: {}", String::from_utf8_lossy(&buffer[..size]));
            stream.write_all(MSG.as_bytes()).unwrap();
        }
        Err(e) => println!("Failed to read from socket: {:?}", e),
    }
}

fn main() {
    // 启动服务器线程
    let server_thread = thread::spawn(|| {
        let listener = UnixListener::bind(SOCKET_PATH).unwrap();
        println!("Server listening on /server.txt");

        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    thread::spawn(|| handle_client(stream));
                }
                Err(err) => {
                    println!("Connection failed: {:?}", err);
                }
            }
        }
    });

    // 确保服务器有时间启动
    thread::sleep(Duration::from_secs(1));
    println!("begin connect");
    // 客户端
    match UnixStream::connect(SOCKET_PATH) {
        Ok(mut stream) => {
            stream.write_all(MSG.as_bytes()).unwrap();
            let mut buffer = [0; 1024];
            match stream.read(&mut buffer) {
                Ok(size) => {
                    println!("Received: {}", String::from_utf8_lossy(&buffer[..size]));
                }
                Err(e) => println!("Failed to read from socket: {:?}", e),
            }
        }
        Err(e) => println!("Failed to connect: {:?}", e),
    }

    // 等待服务器线程结束
    server_thread.join().unwrap();
}