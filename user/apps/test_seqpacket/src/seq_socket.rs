use libc::*;
use std::ffi::CString;
use std::io::Error;
use std::mem;
use std::os::unix::io::RawFd;
use std::{fs, str};

const SOCKET_PATH: &str = "/test.seqpacket";
const MSG1: &str = "Hello, Unix SEQPACKET socket from Client!";
const MSG2: &str = "Hello, Unix SEQPACKET socket from Server!";

fn create_seqpacket_socket() -> Result<RawFd, Error> {
    unsafe {
        let fd = socket(AF_UNIX, SOCK_SEQPACKET, 0);
        if fd == -1 {
            return Err(Error::last_os_error());
        }
        Ok(fd)
    }
}

fn bind_socket(fd: RawFd) -> Result<(), Error> {
    unsafe {
        let mut addr = sockaddr_un {
            sun_family: AF_UNIX as u16,
            sun_path: [0; 108],
        };
        let path_cstr = CString::new(SOCKET_PATH).unwrap();
        let path_bytes = path_cstr.as_bytes();
        for (i, &byte) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = byte as i8;
        }

        if bind(
            fd,
            &addr as *const _ as *const sockaddr,
            mem::size_of_val(&addr) as socklen_t,
        ) == -1
        {
            return Err(Error::last_os_error());
        }
    }
    Ok(())
}

fn listen_socket(fd: RawFd) -> Result<(), Error> {
    unsafe {
        if listen(fd, 5) == -1 {
            return Err(Error::last_os_error());
        }
    }
    Ok(())
}

fn accept_connection(fd: RawFd) -> Result<RawFd, Error> {
    unsafe {
        // let mut addr = sockaddr_un {
        //     sun_family: AF_UNIX as u16,
        //     sun_path: [0; 108],
        // };
        // let mut len = mem::size_of_val(&addr) as socklen_t;
        // let client_fd = accept(fd, &mut addr as *mut _ as *mut sockaddr, &mut len);
        let client_fd = accept(fd, std::ptr::null_mut(), std::ptr::null_mut());
        if client_fd == -1 {
            return Err(Error::last_os_error());
        }
        Ok(client_fd)
    }
}

fn send_message(fd: RawFd, msg: &str) -> Result<(), Error> {
    unsafe {
        let msg_bytes = msg.as_bytes();
        if send(
            fd,
            msg_bytes.as_ptr() as *const libc::c_void,
            msg_bytes.len(),
            0,
        ) == -1
        {
            return Err(Error::last_os_error());
        }
    }
    Ok(())
}

fn receive_message(fd: RawFd) -> Result<String, Error> {
    let mut buffer = [0; 1024];
    unsafe {
        let len = recv(
            fd,
            buffer.as_mut_ptr() as *mut libc::c_void,
            buffer.len(),
            0,
        );
        if len == -1 {
            return Err(Error::last_os_error());
        }
        Ok(String::from_utf8_lossy(&buffer[..len as usize]).into_owned())
    }
}

pub fn test_seq_socket() -> Result<(), Error> {
    // Create and bind the server socket
    fs::remove_file(&SOCKET_PATH).ok();

    let server_fd = create_seqpacket_socket()?;
    bind_socket(server_fd)?;
    listen_socket(server_fd)?;

    // Accept connection in a separate thread
    let server_thread = std::thread::spawn(move || {
        let client_fd = accept_connection(server_fd).expect("Failed to accept connection");

        // Receive and print message
        let received_msg = receive_message(client_fd).expect("Failed to receive message");
        println!("Server: Received message: {}", received_msg);

        send_message(client_fd, MSG2).expect("Failed to send message");

        // Close client connection
        unsafe { close(client_fd) };
    });

    // Create and connect the client socket
    let client_fd = create_seqpacket_socket()?;
    unsafe {
        let mut addr = sockaddr_un {
            sun_family: AF_UNIX as u16,
            sun_path: [0; 108],
        };
        let path_cstr = CString::new(SOCKET_PATH).unwrap();
        let path_bytes = path_cstr.as_bytes();
        // Convert u8 to i8
        for (i, &byte) in path_bytes.iter().enumerate() {
            addr.sun_path[i] = byte as i8;
        }
        if connect(
            client_fd,
            &addr as *const _ as *const sockaddr,
            mem::size_of_val(&addr) as socklen_t,
        ) == -1
        {
            return Err(Error::last_os_error());
        }
    }
    send_message(client_fd, MSG1)?;
    let received_msg = receive_message(client_fd).expect("Failed to receive message");
    println!("Client: Received message: {}", received_msg);
    // get peer_name
    unsafe {
        let mut addrss = sockaddr_un {
            sun_family: AF_UNIX as u16,
            sun_path: [0; 108],
        };
        let mut len = mem::size_of_val(&addrss) as socklen_t;
        let res = getpeername(client_fd, &mut addrss as *mut _ as *mut sockaddr, &mut len);
        if res == -1 {
            return Err(Error::last_os_error());
        }
        let sun_path = addrss.sun_path.clone();
        let peer_path: [u8; 108] = sun_path
            .iter()
            .map(|&x| x as u8)
            .collect::<Vec<u8>>()
            .try_into()
            .unwrap();
        println!(
            "Client: Connected to server at path: {}",
            String::from_utf8_lossy(&peer_path)
        );
    }

    server_thread.join().expect("Server thread panicked");
    let received_msg = receive_message(client_fd).expect("Failed to receive message");
    println!("Client: Received message: {}", received_msg);
    // Close client connection
    unsafe { close(client_fd) };
    fs::remove_file(&SOCKET_PATH).ok();
    Ok(())
}