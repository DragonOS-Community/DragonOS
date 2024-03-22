use std::io::{Error, Read, Write};
use std::os::unix::net::UnixStream;
use std::str::from_utf8;
use std::thread;

const MSG: &str = "Hello, unix stream socketpair!";

fn client(mut sock: UnixStream) -> std::io::Result<()> {
    // 发送消息到对端
    sock.write_all(MSG.as_bytes())?;
    Ok(())
}

pub fn test_unix_stream_pair() -> std::io::Result<()> {
    let (mut sock0, sock1) = UnixStream::pair()?;

    let client_thread = thread::spawn(move || client(sock1));

    let mut buffer = [0; 1024];
    let nbytes = sock0.read(&mut buffer).expect("read error");
    let received_msg = from_utf8(&buffer[..nbytes]).unwrap();

    if client_thread.join().is_err() {
        return Err(Error::from_raw_os_error(-2));
    }

    if received_msg == MSG {
        Ok(())
    } else {
        Err(Error::from_raw_os_error(-1))
    }
}
