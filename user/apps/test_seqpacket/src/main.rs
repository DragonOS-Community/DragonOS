mod seq_pair;
mod seq_socket;

use seq_pair::test_seq_pair;
use seq_socket::test_seq_socket;

fn main() -> Result<(), std::io::Error> {
    if let Err(e) = test_seq_socket() {
        println!("[ fault ] test_seq_socket, err: {}", e);
    } else {
        println!("[success] test_seq_socket");
    }

    if let Err(e) = test_seq_pair() {
        println!("[ fault ] test_seq_pair, err: {}", e);
    } else {
        println!("[success] test_seq_pair");
    }

    Ok(())
}

// use nix::sys::socket::{socketpair, AddressFamily, SockFlag, SockType};
// use std::fs::File;
// use std::io::{Read, Write};
// use std::os::fd::FromRawFd;
// use std::{fs, str};

// use libc::*;
// use std::ffi::CString;
// use std::io::Error;
// use std::mem;
// use std::os::unix::io::RawFd;
// use std::ptr;

// const SOCKET_PATH: &str = "/test.seqpacket";
// const MSG: &str = "Hello, Unix SEQPACKET socket!";

// fn create_seqpacket_socket() -> Result<RawFd, Error> {
//     unsafe {
//         let fd = socket(AF_UNIX, SOCK_SEQPACKET, 0);
//         if fd == -1 {
//             return Err(Error::last_os_error());
//         }
//         Ok(fd)
//     }
// }

// fn bind_socket(fd: RawFd) -> Result<(), Error> {
//     unsafe {
//         let mut addr = sockaddr_un {
//             sun_family: AF_UNIX as u16,
//             sun_path: [0; 108],
//         };
//         let path_cstr = CString::new(SOCKET_PATH).unwrap();
//         let path_bytes = path_cstr.as_bytes();
//         for (i, &byte) in path_bytes.iter().enumerate() {
//             addr.sun_path[i] = byte as i8;
//         }

//         if bind(fd, &addr as *const _ as *const sockaddr, mem::size_of_val(&addr) as socklen_t) == -1 {
//             return Err(Error::last_os_error());
//         }
//     }
//     Ok(())
// }

// fn listen_socket(fd: RawFd) -> Result<(), Error> {
//     unsafe {
//         if listen(fd, 5) == -1 {
//             return Err(Error::last_os_error());
//         }
//     }
//     Ok(())
// }

// fn accept_connection(fd: RawFd) -> Result<RawFd, Error> {
//     unsafe {
//         // let mut addr = sockaddr_un {
//         //     sun_family: AF_UNIX as u16,
//         //     sun_path: [0; 108],
//         // };
//         // let mut len = mem::size_of_val(&addr) as socklen_t;
//         let client_fd = accept(fd, std::ptr::null_mut(), std::ptr::null_mut());
//         if client_fd == -1 {
//             return Err(Error::last_os_error());
//         }
//         Ok(client_fd)
//     }
// }

// fn send_message(fd: RawFd, msg: &str) -> Result<(), Error> {
//     unsafe {
//         let msg_bytes = msg.as_bytes();
//         if send(fd, msg_bytes.as_ptr() as *const libc::c_void, msg_bytes.len(), 0) == -1 {
//             return Err(Error::last_os_error());
//         }
//     }
//     Ok(())
// }

// fn receive_message(fd: RawFd) -> Result<String, Error> {
//     let mut buffer = [0; 1024];
//     unsafe {
//         let len = recv(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len(), 0);
//         if len == -1 {
//             return Err(Error::last_os_error());
//         }
//         Ok(String::from_utf8_lossy(&buffer[..len as usize]).into_owned())
//     }
// }
// fn main() -> Result<(), Error> {
//     // Create and bind the server socket
//     fs::remove_file(&SOCKET_PATH).ok();

//     let server_fd = create_seqpacket_socket()?;
//     bind_socket(server_fd)?;
//     listen_socket(server_fd)?;

//     // Accept connection in a separate thread
//     let server_thread = std::thread::spawn(move || {
//         let client_fd = accept_connection(server_fd).expect("Failed to accept connection");

//         // Receive and print message
//         let received_msg = receive_message(client_fd).expect("Failed to receive message");
//         println!("Server: Received message: {}", received_msg);

//         // Close client connection
//         unsafe { close(client_fd) };
//     });

//     // Create and connect the client socket
//     let client_fd = create_seqpacket_socket()?;
//     unsafe {
//         let mut addr = sockaddr_un {
//             sun_family: AF_UNIX as u16,
//             sun_path: [0; 108],
//         };
//         let path_cstr = CString::new(SOCKET_PATH).unwrap();
//         let path_bytes = path_cstr.as_bytes();
//         // Convert u8 to i8
//         for (i, &byte) in path_bytes.iter().enumerate() {
//             addr.sun_path[i] = byte as i8;
//         }
//         if connect(client_fd, &addr as *const _ as *const sockaddr, mem::size_of_val(&addr) as socklen_t) == -1 {
//             return Err(Error::last_os_error());
//         }
//     }
//     send_message(client_fd, MSG)?;

//     // Close client connection
//     unsafe { close(client_fd) };

//     // Wait for server thread to complete
//     server_thread.join().expect("Server thread panicked");
//     fs::remove_file(&SOCKET_PATH).ok();

//         // 创建 socket pair
//     let (sock1, sock2) = socketpair(
//         AddressFamily::Unix,
//         SockType::SeqPacket, // 使用 SeqPacket 类型
//         None,                // 协议默认
//         SockFlag::empty(),
//     ).expect("Failed to create socket pair");

//     let mut socket1 = unsafe { File::from_raw_fd(sock1) };
//     let mut socket2 = unsafe { File::from_raw_fd(sock2) };
//     // sock1 写入数据
//     let msg = b"hello from sock1";
//     socket1.write_all(msg)?;
//     println!("sock1 send: {:?}", String::from_utf8_lossy(&msg[..]));

//     // 因os read和write时会调整file的offset,write会对offset和meta size(目前返回的都是0)进行比较，
//     // 而read不会，故双socket都先send,后recv

//     // sock2 回复数据
//     let reply = b"hello from sock2";
//     socket2.write_all(reply)?;
//     println!("sock2 send: {:?}", String::from_utf8_lossy(reply));

//     // sock2 读取数据
//     let mut buf = [0u8; 128];
//     let len = socket2.read(&mut buf)?;
//     println!("sock2 receive: {:?}", String::from_utf8_lossy(&buf[..len]));

//     // sock1 读取回复
//     let len = socket1.read(&mut buf)?;
//     println!("sock1 receive: {:?}", String::from_utf8_lossy(&buf[..len]));
//     Ok(())
// }
