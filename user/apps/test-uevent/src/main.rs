use libc::{sockaddr,  recvfrom, bind, sendto, socket, AF_NETLINK, SOCK_DGRAM, getpid, c_void};
use nix::libc;
use std::os::unix::io::RawFd;
use std::{ mem, io};

#[repr(C)]
struct Nlmsghdr {
    nlmsg_len: u32,
    nlmsg_type: u16,
    nlmsg_flags: u16,
    nlmsg_seq: u32,
    nlmsg_pid: u32,
}

fn create_netlink_socket() -> io::Result<RawFd> {
    let sockfd = unsafe {
        socket(AF_NETLINK, SOCK_DGRAM, libc::NETLINK_KOBJECT_UEVENT)
    };

    if sockfd < 0 {
        println!("Error: {}", io::Error::last_os_error());
        return Err(io::Error::last_os_error());
    }

    Ok(sockfd)
}

fn bind_netlink_socket(sock: RawFd) -> io::Result<()> {
    let pid = unsafe { getpid() };
    let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
    addr.nl_family = AF_NETLINK as u16;
    addr.nl_pid = pid as u32;
    addr.nl_groups = 1;

    let ret = unsafe {
        bind(sock, &addr as *const _ as *const sockaddr, mem::size_of::<libc::sockaddr_nl>() as u32)
    };

    if ret < 0 {
        println!("Error: {}", io::Error::last_os_error());
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn send_uevent(sock: RawFd, message: &str) -> io::Result<()> {
    let mut addr: libc::sockaddr_nl = unsafe { mem::zeroed() };
    addr.nl_family = AF_NETLINK as u16;
    addr.nl_pid = 0;
    addr.nl_groups = 0;

    let nlmsghdr = Nlmsghdr {
        nlmsg_len: (mem::size_of::<Nlmsghdr>() + message.len()) as u32,
        nlmsg_type: 0,
        nlmsg_flags: 0,
        nlmsg_seq: 0,
        nlmsg_pid: 0,
    };

    let mut buffer = Vec::with_capacity(nlmsghdr.nlmsg_len as usize);
    buffer.extend_from_slice(unsafe {
        std::slice::from_raw_parts(
            &nlmsghdr as *const Nlmsghdr as *const u8,
            mem::size_of::<Nlmsghdr>(),
        )
    });
    buffer.extend_from_slice(message.as_bytes());

    let ret = unsafe {
        sendto(
            sock,
            buffer.as_ptr() as *const c_void,
            buffer.len(),
            0,
            &addr as *const _ as *const sockaddr,
            mem::size_of::<libc::sockaddr_nl>() as u32,
        )
    };

    if ret < 0 {
        println!("Error: {}", io::Error::last_os_error());
        return Err(io::Error::last_os_error());
    }

    Ok(())
}

fn receive_uevent(sock: RawFd) -> io::Result<String> {
    // 检查套接字文件描述符是否有效
    if sock < 0 {
        println!("Invalid socket file descriptor: {}", sock);
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid socket file descriptor"));
    }

    let mut buf = [0u8; 1024];
    // let mut addr: sockaddr_storage = unsafe { mem::zeroed() };
    // let mut addr_len = mem::size_of::<sockaddr_storage>() as u32;

    // 检查缓冲区指针和长度是否有效
    if buf.is_empty() {
        println!("Buffer is empty");
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Buffer is empty"));
    }
    let len = unsafe {
        recvfrom(
            sock,
            buf.as_mut_ptr() as *mut c_void,
            buf.len(),
            0,
            core::ptr::null_mut(), // 不接收发送方地址
            core::ptr::null_mut(), // 不接收发送方地址长度
        )
    };
    println!("Received {} bytes", len);
    println!("Received message: {:?}", &buf[..len as usize]);
    if len < 0 {
        println!("Error: {}", io::Error::last_os_error());
        return Err(io::Error::last_os_error());
    }

    let nlmsghdr_size = mem::size_of::<Nlmsghdr>();
    if (len as usize) < nlmsghdr_size {
        println!("Received message is too short");
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Received message is too short"));
    }

    let nlmsghdr = unsafe { &*(buf.as_ptr() as *const Nlmsghdr) };
    if nlmsghdr.nlmsg_len as isize > len {
        println!("Received message is incomplete");
        return Err(io::Error::new(io::ErrorKind::InvalidData, "Received message is incomplete"));
    }

    let message_data = &buf[nlmsghdr_size..nlmsghdr.nlmsg_len as usize];
    Ok(String::from_utf8_lossy(message_data).to_string())
}

fn main() {
    let socket = create_netlink_socket().expect("Failed to create Netlink socket");
    println!("Netlink socket created successfully");

    bind_netlink_socket(socket).expect("Failed to bind Netlink socket");
    println!("Netlink socket created and bound successfully");

    send_uevent(socket, "add@/devices/virtual/block/loop0").expect("Failed to send uevent message");
    println!("Custom uevent message sent successfully");

    let message = receive_uevent(socket).expect("Failed to receive uevent message");
    println!("Received uevent message: {}", message);
}
