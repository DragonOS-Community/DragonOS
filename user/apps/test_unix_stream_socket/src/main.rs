use std::io::Error;
use std::os::fd::RawFd;
use std::fs;
use libc::*;
use std::ffi::CString;
use std::mem;

const SOCKET_PATH: &str = "/test.stream";
const MSG1: &str = "Hello, unix stream socket from Client!";
const MSG2: &str = "Hello, unix stream socket from Server!";

fn create_stream_socket() -> Result<RawFd, Error>{
    unsafe {
        let fd = socket(AF_UNIX, SOCK_STREAM, 0);
        if fd == -1 {
            return Err(Error::last_os_error())
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

        if bind(fd, &addr as *const _ as *const sockaddr, mem::size_of_val(&addr) as socklen_t) == -1 {
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

fn accept_conn(fd: RawFd) -> Result<RawFd, Error> {
    unsafe {
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
        if send(fd, msg_bytes.as_ptr() as *const libc::c_void, msg_bytes.len(), 0)== -1 {
            return Err(Error::last_os_error());
        }
    }
    Ok(())
}

fn recv_message(fd: RawFd) -> Result<String, Error> {
    let mut buffer = [0; 1024];
    unsafe {
        let len = recv(fd, buffer.as_mut_ptr() as *mut libc::c_void, buffer.len(),0);
        if len == -1 {
            return Err(Error::last_os_error());
        }
        Ok(String::from_utf8_lossy(&buffer[..len as usize]).into_owned())
    }
}

fn test_stream() -> Result<(), Error> {
    fs::remove_file(&SOCKET_PATH).ok();

    let server_fd =  create_stream_socket()?;
    bind_socket(server_fd)?;
    listen_socket(server_fd)?;

    let server_thread = std::thread::spawn(move || {
        let client_fd = accept_conn(server_fd).expect("Failed to accept connection");
        println!("accept success!");
        let recv_msg = recv_message(client_fd).expect("Failed to receive message");

        println!("Server: Received message: {}", recv_msg);
        send_message(client_fd, MSG2).expect("Failed to send message");
        println!("Server send finish");

        unsafe {close(client_fd)};
    });

    let client_fd = create_stream_socket()?;
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

        if connect(client_fd, &addr as *const _ as *const sockaddr, mem::size_of_val(&addr) as socklen_t) == -1 {
            return Err(Error::last_os_error());
        } 
    }

    send_message(client_fd, MSG1)?;
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
        let peer_path:[u8;108] = sun_path.iter().map(|&x| x as u8).collect::<Vec<u8>>().try_into().unwrap();
        println!("Client: Connected to server at path: {}", String::from_utf8_lossy(&peer_path));

    }

    server_thread.join().expect("Server thread panicked");
    println!("Client try recv!");
    let recv_msg = recv_message(client_fd).expect("Failed to receive message from server");
    println!("Client Received message: {}", recv_msg);

    unsafe {close(client_fd)};
    fs::remove_file(&SOCKET_PATH).ok();

    Ok(())
}

fn main() {
    match test_stream() {
        Ok(_) => println!("test for unix stream success"),
        Err(_) => println!("test for unix stream failed")
    }
}