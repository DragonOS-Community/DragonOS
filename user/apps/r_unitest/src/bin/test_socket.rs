use test_unix_stream::test_unix_stream;
use test_unix_stream_pair::test_unix_stream_pair;

fn main() -> std::io::Result<()> {
    if let Err(e) = test_unix_stream() {
        println!("[ fault ] test_unix_stream, err: {}", e);
    } else {
        println!("[success] test_unix_stream");
    }

    if let Err(e) = test_unix_stream_pair() {
        println!("[ fault ] test_unix_stream_pair, err: {}", e);
    } else {
        println!("[success] test_unix_stream_pair");
    }

    Ok(())
}

mod test_unix_stream_pair {
    use std::io::{Error, Read, Write};
    use std::os::unix::net::UnixStream;
    use std::str;

    const MSG: &str = "Hello, unix stream socketpair!";

    pub fn test_unix_stream_pair() -> std::io::Result<()> {
        let (mut sock0, mut sock1) = UnixStream::pair()?;

        sock1.write_all(MSG.as_bytes())?;

        let mut buffer = [0; 1024];
        let nbytes = sock0.read(&mut buffer).expect("read error");
        let received_msg = str::from_utf8(&buffer[..nbytes]).unwrap();

        if received_msg == MSG {
            Ok(())
        } else {
            Err(Error::from_raw_os_error(-1))
        }
    }
}

mod test_unix_stream {
    use std::io::{Error, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;
    use std::{fs, str};

    const SOCKET_PATH: &str = "/test.socket";
    const MSG: &str = "Hello, unix stream socket!";

    fn client() -> std::io::Result<()> {
        // 连接到服务器
        let mut stream = UnixStream::connect(SOCKET_PATH)?;
        // 发送消息到服务器
        stream.write_all(MSG.as_bytes())?;
        Ok(())
    }

    pub fn test_unix_stream() -> std::io::Result<()> {
        println!("unix stream socket path: {}", SOCKET_PATH);
        // 删除可能已存在的socket文件
        fs::remove_file(&SOCKET_PATH).ok();
        // 创建Unix域监听socket
        let listener = UnixListener::bind(SOCKET_PATH)?;

        let client_thread = thread::spawn(move || client());

        // 监听并接受连接
        let (mut stream, _) = listener.accept().expect("listen error");

        let mut buffer = [0; 1024];
        let nbytes = stream.read(&mut buffer).expect("read error");
        let received_msg = str::from_utf8(&buffer[..nbytes]).unwrap();

        client_thread.join().ok();

        fs::remove_file(&SOCKET_PATH).ok();

        if received_msg == MSG {
            Ok(())
        } else {
            Err(Error::from_raw_os_error(-1))
        }
    }
}
