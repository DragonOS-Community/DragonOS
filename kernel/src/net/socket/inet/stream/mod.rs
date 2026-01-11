use alloc::sync::Arc;
use core::sync::atomic::AtomicUsize;
use system_error::SystemError;

use crate::filesystem::vfs::iov::IoVecs;
use crate::filesystem::vfs::{fasync::FAsyncItems, InodeId};
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::net::socket::posix::IpOption;
use crate::net::socket::unix::utils::{CmsgBuffer, SOL_SOCKET};
use crate::net::socket::{common::ShutdownBit, endpoint::Endpoint, Socket, PMSG, PSO, PSOL};
use crate::time::syscall::PosixTimeval;

mod constants;
mod info;
mod inner;
mod option;
pub use option::Options as TcpOption;
use option::Options;

use super::{InetSocket, UNSPECIFIED_LOCAL_ENDPOINT_V4};

type EP = crate::filesystem::epoll::EPollEventType;

mod events;
mod io;
mod lifecycle;
mod poll_util;
mod stream_core;

pub use stream_core::TcpSocket;

impl Socket for TcpSocket {
    fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblock
            .store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }

    fn recvfrom_addr_behavior(&self) -> crate::net::socket::RecvFromAddrBehavior {
        // Linux/gVisor: for TCP (SOCK_STREAM), recvfrom(2) ignores the source
        // address output parameters. If addrlen is provided, kernel writes back 0.
        crate::net::socket::RecvFromAddrBehavior::Ignore
    }

    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let inner = self.inner.read();
        let inner = inner.as_ref().ok_or(SystemError::ENOTCONN)?;
        Ok(Endpoint::Ip(inner.local_endpoint()))
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        let inner = self.inner.read();
        let inner = inner.as_ref().ok_or(SystemError::ENOTCONN)?;
        inner
            .remote_endpoint()
            .map(Endpoint::Ip)
            .ok_or(SystemError::ENOTCONN)
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match level {
            PSOL::IP => {
                let optname =
                    IpOption::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_ip_option(optname, value)
            }
            PSOL::TCP => {
                let opt = Options::try_from(name as i32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_tcp_option(opt, value)
            }
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_socket_option(opt, value)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn bind(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(addr) = endpoint {
            return self.do_bind(addr);
        }
        // log::debug!("TcpSocket::bind: invalid endpoint");
        return Err(SystemError::EINVAL);
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        let Endpoint::Ip(endpoint) = endpoint else {
            // log::debug!("TcpSocket::connect: invalid endpoint");
            return Err(SystemError::EINVAL);
        };
        self.start_connect(endpoint)?; // Only Nonblock or error will return error.

        return loop {
            match self.check_connect() {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    // log::debug!("TcpSocket::connect: wait for Established");
                    wq_wait_event_interruptible!(
                        self.wait_queue(),
                        {
                            // 关键：不要等待 `self.inner` 变成 Established。
                            // `self.inner` 的状态转换是由 check_connect() 完成的；
                            // 如果 iface.poll() 在入睡前已经推进到 Established 并触发过 notify/wakeup，
                            // 再等 “inner 已 Established” 会陷入先唤后睡的丢唤醒。
                            // 这里把 check_connect() 本身作为条件的一部分：
                            // - 入队 waker 后再次检查时会主动 poll 并完成状态转换
                            // - 即使错过了一次 wake，也不会永远睡下去
                            !matches!(
                                self.check_connect(),
                                Err(SystemError::EAGAIN_OR_EWOULDBLOCK)
                            )
                        },
                        {}
                    )?;
                }
                result => {
                    // log::debug!("TcpSocket::connect: done -> {:?}", result);
                    break result;
                }
            }
        };
    }

    fn listen(&self, backlog: usize) -> Result<(), SystemError> {
        self.do_listen(backlog)
    }

    fn accept(&self) -> Result<(Arc<dyn Socket>, Endpoint), SystemError> {
        if self.is_nonblock() {
            self.try_accept()
        } else {
            loop {
                match self.try_accept() {
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        wq_wait_event_interruptible!(self.wait_queue, self.incoming(), {})?;
                    }
                    result => break result,
                }
            }
        }
        .map(|(sock, ep)| (sock as Arc<dyn Socket>, Endpoint::Ip(ep)))
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        if self.is_recv_shutdown() {
            return Ok(0);
        }

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            return self.try_recv_with_flags(buffer, flags);
        }

        loop {
            match self.try_recv_with_flags(buffer, flags) {
                Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                    // Poll in a loop until no more events. This is critical for loopback:
                    // - Poll 1: TX sends data to loopback queue, RX processes existing packets
                    // - Poll 2: RX processes the data we just transmitted (loopback roundtrip)
                    // Without this loop, we'd wait for the poll thread to complete the roundtrip.
                    if let Some(iface) = self.inner.read().as_ref().and_then(|i| i.iface()).cloned()
                    {
                        poll_util::poll_iface_until_quiescent(iface.as_ref());
                    }
                    // After polling, check if EPOLLIN is now set before waiting.
                    // update_events() was called by poll() -> notify(), so pollee is fresh.
                    if EP::from_bits_truncate(
                        self.pollee.load(core::sync::atomic::Ordering::SeqCst) as u32,
                    )
                    .contains(EP::EPOLLIN)
                    {
                        continue; // Data available now, retry recv
                    }
                    // Wait for EPOLLIN. The poll thread's notify() updates pollee after polling.
                    self.wait_queue.wait_event_interruptible_timeout(
                        || {
                            EP::from_bits_truncate(
                                self.pollee.load(core::sync::atomic::Ordering::SeqCst) as u32,
                            )
                            .contains(EP::EPOLLIN)
                        },
                        self.recv_timeout(),
                    )?;
                }
                result => break result,
            }
        }
    }

    fn send(&self, buffer: &[u8], _flags: PMSG) -> Result<usize, SystemError> {
        if buffer.is_empty() {
            // Linux 语义：write(fd, "", 0) / send(fd, ..., 0) 直接返回 0。
            return Ok(0);
        }

        if self.is_nonblock() || _flags.contains(PMSG::DONTWAIT) {
            return self.try_send(buffer);
        }

        // 先尝试写一次：写到多少就返回多少（允许短写）。
        match self.try_send(buffer) {
            Ok(n) => return Ok(n),
            Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => { /* fallthrough: block */ }
            Err(e) => return Err(e),
        }

        loop {
            // loopback 场景需要把协议栈推进到“真正可写/不可写”的稳定状态，避免丢唤醒。
            if let Some(iface) = self.inner.read().as_ref().and_then(|i| i.iface()).cloned() {
                poll_util::poll_iface_until_quiescent(iface.as_ref());
            }

            // 若已经可写，重试一次并直接返回（允许短写）。
            if EP::from_bits_truncate(self.pollee.load(core::sync::atomic::Ordering::SeqCst) as u32)
                .contains(EP::EPOLLOUT)
            {
                return self.try_send(buffer);
            }

            // 等待可写或超时/信号。
            self.wait_queue.wait_event_interruptible_timeout(
                || {
                    EP::from_bits_truncate(
                        self.pollee.load(core::sync::atomic::Ordering::SeqCst) as u32
                    )
                    .contains(EP::EPOLLOUT)
                },
                self.send_timeout(),
            )?;
        }
    }

    fn send_buffer_size(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .expect("Tcp inner::Inner is None")
            .send_buffer_size()
    }

    fn recv_buffer_size(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .expect("Tcp inner::Inner is None")
            .recv_buffer_size()
    }

    fn recv_bytes_available(&self) -> usize {
        // Linux ioctl(FIONREAD/TIOCINQ) on TCP sockets reports the number of bytes
        // currently in the receive queue. For non-established sockets, report 0.
        self.recv_queue_len()
    }

    fn shutdown(&self, how: ShutdownBit) -> Result<(), SystemError> {
        self.do_shutdown(how)
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }

    fn do_close(&self) -> Result<(), SystemError> {
        self.close_socket()
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::IP => {
                let opt = crate::net::socket::IpOption::try_from(name as u32)
                    .map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_ip_option(opt, val)
            }
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_socket_option(opt, val)
            }
            PSOL::TCP => {
                let opt = option::Options::try_from(name as i32)?;
                // log::debug!("TCP Option: {:?}, value = {:?}", opt, val);
                self.set_tcp_option(opt, val)
            }
            _ => Ok(()),
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // Linux 语义：对 SOCK_STREAM(TCP) 的 recvfrom(2)，addr 参数被忽略。
        // “不写回 sockaddr” 的行为在 syscall 层做特殊处理，避免修改用户缓冲区。
        let n = self.recv(buffer, flags)?;
        // 返回值仅用于统一接口；不会被写回给用户。
        let ep = self.remote_endpoint().unwrap_or_else(|_| {
            self.local_endpoint()
                .unwrap_or(Endpoint::Ip(UNSPECIFIED_LOCAL_ENDPOINT_V4))
        });
        Ok((n, ep))
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        // TCP: 不返回 peer 地址。
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let total = iovs.total_len();
        let mut tmp = vec![0u8; total];
        let n = self.recv(&mut tmp, flags)?;
        let written = iovs.scatter(&tmp[..n])?;

        msg.msg_flags = 0;
        msg.msg_namelen = 0;

        // Ancillary data: SO_TIMESTAMP and TCP_INQ.
        let mut cmsg_write_off = 0usize;
        if !msg.msg_control.is_null() && msg.msg_controllen != 0 {
            let mut cbuf = CmsgBuffer {
                ptr: msg.msg_control,
                len: msg.msg_controllen,
                write_off: &mut cmsg_write_off,
            };

            if self.timestamp_enabled() {
                let now_ns = crate::time::timekeeping::do_gettimeofday().to_ns();
                let tv = PosixTimeval::from_ns(now_ns);
                let tv_bytes: &[u8] = unsafe {
                    core::slice::from_raw_parts(
                        (&tv as *const PosixTimeval) as *const u8,
                        core::mem::size_of::<PosixTimeval>(),
                    )
                };
                // SCM_TIMESTAMP uses the same numeric value as SO_TIMESTAMP (29).
                let cmsg_type = PSO::TIMESTAMP_OLD as i32;
                cbuf.put(
                    &mut msg.msg_flags,
                    SOL_SOCKET,
                    cmsg_type,
                    core::mem::size_of::<PosixTimeval>(),
                    tv_bytes,
                )?;
            }

            if self.inq_enabled() {
                let inq = TcpSocket::clamp_usize_to_i32(self.recv_queue_len());
                let inq_bytes = inq.to_ne_bytes();
                cbuf.put(
                    &mut msg.msg_flags,
                    PSOL::TCP as i32,
                    option::Options::INQ as i32,
                    core::mem::size_of::<i32>(),
                    &inq_bytes,
                )?;
            }
        }

        msg.msg_controllen = cmsg_write_off;
        Ok(written)
    }

    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        // TCP: msg_name 作为目的地址被忽略，等价于 send(2)。
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let data = iovs.gather()?;
        self.send(&data, flags)
    }

    fn send_to(
        &self,
        buffer: &[u8],
        flags: PMSG,
        _address: Endpoint,
    ) -> Result<usize, SystemError> {
        // Linux 语义：对已连接 SOCK_STREAM(TCP)，sendto(2) 的地址参数被忽略。
        self.send(buffer, flags)
    }

    fn epoll_items(&self) -> &EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> crate::filesystem::epoll::EPollEventType {
        self.update_events();
        EP::from_bits_truncate(self.do_poll() as u32)
    }

    fn ioctl(
        &self,
        _cmd: u32,
        _arg: usize,
        _private_data: &crate::filesystem::vfs::FilePrivateData,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
}

impl InetSocket for TcpSocket {
    fn on_iface_events(&self) {
        // Iface::poll() 在网络轮询线程/中断上下文中推进 smoltcp socket 状态。
        // 这里负责把 smoltcp 的状态变化同步到 TcpSocket 的 pollee/Connecting 结果中，
        // 以便 connect/accept/epoll 等等待者能被正确唤醒并观察到状态前进。
        //
        // 重要：driver/net/mod.rs 已保证 notify() 调用时不再持有 bounds 读锁，
        // 因此这里可以安全地获取 self.inner 的 RwLock 并更新事件。
        let _ = self.update_events();
    }
}
