use std::io::{Error, Read, Write};
use std::os::unix::net::UnixStream;
use std::str;

use crate::Test;

const MSG: &str = "Hello, unix stream socketpair!";

fn test_unix_stream_pair() -> std::io::Result<()> {
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

impl Test {
    pub fn test_unix_stream_pair() -> std::io::Result<()> {
        test_unix_stream_pair()
    }
}
