use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
use std::fs::File;
use std::io::{Error, Read, Write};
use std::os::fd::FromRawFd;

pub fn test_seq_pair() -> Result<(), Error> {
    // 创建 socket pair
    let (sock1, sock2) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket, // 使用 SeqPacket 类型
        None,                // 协议默认
        SockFlag::empty(),
    )
    .expect("Failed to create socket pair");

    let mut socket1 = unsafe { File::from_raw_fd(sock1) };
    let mut socket2 = unsafe { File::from_raw_fd(sock2) };
    // sock1 写入数据
    let msg = b"hello from sock1";
    socket1.write_all(msg)?;
    println!("sock1 send: {:?}", String::from_utf8_lossy(&msg[..]));

    // 因os read和write时会调整file的offset,write会对offset和meta size(目前返回的都是0)进行比较，
    // 而read不会，故双socket都先send,后recv

    // sock2 回复数据
    let reply = b"hello from sock2";
    socket2.write_all(reply)?;
    println!("sock2 send: {:?}", String::from_utf8_lossy(reply));

    // sock2 读取数据
    let mut buf = [0u8; 128];
    let len = socket2.read(&mut buf)?;
    println!("sock2 receive: {:?}", String::from_utf8_lossy(&buf[..len]));

    // sock1 读取回复
    let len = socket1.read(&mut buf)?;
    println!("sock1 receive: {:?}", String::from_utf8_lossy(&buf[..len]));
    Ok(())
}
