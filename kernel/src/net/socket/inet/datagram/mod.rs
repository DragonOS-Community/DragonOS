use inner::{UdpInner, UnboundUdp, DEFAULT_RX_BUF_SIZE, DEFAULT_TX_BUF_SIZE};
use smoltcp;
use system_error::SystemError;

use crate::filesystem::epoll::EPollEventType;
use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::{EPollItems, ShutdownBit};
use crate::net::socket::{Socket, PSO, PMSG, PSOL};
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;
use crate::{libs::rwlock::RwLock, net::socket::endpoint::Endpoint};
use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

use super::InetSocket;

pub mod inner;

type EP = crate::filesystem::epoll::EPollEventType;

// Udp Socket 负责提供状态切换接口、执行状态切换
#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct UdpSocket {
    inner: RwLock<Option<UdpInner>>,
    nonblock: AtomicBool,
    shutdown: AtomicU8,
    wait_queue: WaitQueue,
    inode_id: InodeId,
    open_files: AtomicUsize,
    self_ref: Weak<UdpSocket>,
    netns: Arc<NetNamespace>,
    epoll_items: EPollItems,
    fasync_items: FAsyncItems,
    /// Custom send buffer size (SO_SNDBUF), 0 means use default
    send_buf_size: AtomicUsize,
    /// Custom receive buffer size (SO_RCVBUF), 0 means use default
    recv_buf_size: AtomicUsize,
}

impl UdpSocket {
    pub fn new(nonblock: bool) -> Arc<Self> {
        let netns = ProcessManager::current_netns();
        Arc::new_cyclic(|me| Self {
            inner: RwLock::new(Some(UdpInner::Unbound(UnboundUdp::new()))),
            nonblock: AtomicBool::new(nonblock),
            shutdown: AtomicU8::new(0),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            send_buf_size: AtomicUsize::new(0), // 0 means use default
            recv_buf_size: AtomicUsize::new(0), // 0 means use default
        })
    }

    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn do_bind(&self, local_endpoint: smoltcp::wire::IpEndpoint) -> Result<(), SystemError> {
        let mut inner = self.inner.write();

        // Check socket state first without taking
        match inner.as_ref() {
            None => return Err(SystemError::EBADF),
            Some(UdpInner::Bound(_)) => return Err(SystemError::EINVAL), // Already bound
            Some(UdpInner::Unbound(_)) => {}
        }

        // Now safe to take - we know it's Unbound
        let unbound = match inner.take() {
            Some(UdpInner::Unbound(unbound)) => unbound,
            _ => unreachable!(),
        };

        match unbound.bind(local_endpoint, self.netns()) {
            Ok(bound) => {
                bound
                    .inner()
                    .iface()
                    .common()
                    .bind_socket(self.self_ref.upgrade().unwrap());
                *inner = Some(UdpInner::Bound(bound));
                Ok(())
            }
            Err(e) => {
                // Restore unbound state on error
                *inner = Some(UdpInner::Unbound(UnboundUdp::new()));
                Err(e)
            }
        }
    }

    pub fn bind_ephemeral(&self, remote: smoltcp::wire::IpAddress) -> Result<(), SystemError> {
        let mut inner_guard = self.inner.write();
        let inner = inner_guard.take().ok_or(SystemError::EBADF)?;
        let bound = match inner {
            UdpInner::Bound(inner) => inner,
            UdpInner::Unbound(inner) => match inner.bind_ephemeral(remote, self.netns()) {
                Ok(bound) => bound,
                Err(e) => {
                    inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                    return Err(e);
                }
            },
        };
        inner_guard.replace(UdpInner::Bound(bound));
        Ok(())
    }

    pub fn is_bound(&self) -> bool {
        let inner = self.inner.read();
        if let Some(UdpInner::Bound(_)) = &*inner {
            return true;
        }
        return false;
    }

    pub fn close(&self) {
        let mut inner = self.inner.write();
        if let Some(UdpInner::Bound(bound)) = &mut *inner {
            bound.close();
            inner.take();
        }
        // unbound socket just drop (only need to free memory)
    }

    pub fn try_recv(
        &self,
        buf: &mut [u8],
    ) -> Result<(usize, smoltcp::wire::IpEndpoint), SystemError> {
        match self.inner.read().as_ref().ok_or(SystemError::EBADF)? {
            UdpInner::Bound(bound) => {
                // Poll BEFORE try_recv to receive any pending packets
                let iface = bound.inner().iface();
                iface.poll();

                // Try to receive
                let result = bound.try_recv(buf);

                // For loopback packets, if we got EAGAIN, poll a few more times
                // to allow packets to propagate through the loopback interface
                if matches!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)) {
                    // Check if bound to loopback address
                    let local_endpoint = bound.endpoint();
                    let is_loopback = if let Some(addr) = local_endpoint.addr {
                        matches!(addr, smoltcp::wire::IpAddress::Ipv4(ipv4) if ipv4.octets()[0] == 127)
                    } else {
                        false
                    };

                    if is_loopback {
                        // Poll up to 5 more times for loopback to ensure packet delivery
                        for _ in 0..5 {
                            iface.poll();
                            let retry = bound.try_recv(buf);
                            if !matches!(retry, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)) {
                                return retry;
                            }
                        }
                    }
                }

                result
            }
            // UDP is connectionless - unbound socket just has no data yet
            UdpInner::Unbound(_) => Err(SystemError::EAGAIN_OR_EWOULDBLOCK),
        }
    }

    #[inline]
    pub fn can_recv(&self) -> bool {
        // Can receive if there's data available OR if read is shutdown
        // (shutdown should wake up recv() to return 0/EOF)
        let has_data = self.check_io_event().contains(EP::EPOLLIN);
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let read_shutdown = (shutdown_bits & 0x01) != 0;
        has_data || read_shutdown
    }

    #[inline]
    #[allow(dead_code)]
    pub fn can_send(&self) -> bool {
        // Can send if socket is ready OR if write is shutdown
        // (shutdown should wake up send() to return EPIPE)
        let can_write = self.check_io_event().contains(EP::EPOLLOUT);
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let write_shutdown = (shutdown_bits & 0x02) != 0;
        can_write || write_shutdown
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        // Poll first to process any pending network events and free buffers
        let iface = {
            match self.inner.read().as_ref() {
                Some(UdpInner::Bound(bound)) => Some(bound.inner().iface().clone()),
                _ => None,
            }
        };

        if let Some(iface) = &iface {
            iface.poll();
        }

        // Send data and get iface reference, then release lock before polling
        let (result, iface) = {
            let mut inner_guard = self.inner.write();

            // Check if socket is closed
            let inner = inner_guard.as_ref().ok_or(SystemError::EBADF)?;

            // If unbound, bind to ephemeral port
            if let UdpInner::Unbound(_) = inner {
                let to_addr = to.ok_or(SystemError::EDESTADDRREQ)?.addr;
                let unbound = match inner_guard.take().unwrap() {
                    UdpInner::Unbound(unbound) => unbound,
                    _ => unreachable!(),
                };
                match unbound.bind_ephemeral(to_addr, self.netns()) {
                    Ok(bound) => {
                        inner_guard.replace(UdpInner::Bound(bound));
                    }
                    Err(e) => {
                        // Restore unbound state on error
                        inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                        return Err(e);
                    }
                }
            }

            // Send data and get iface Arc before releasing lock
            match inner_guard.as_ref().ok_or(SystemError::EBADF)? {
                UdpInner::Bound(bound) => {
                    let ret = bound.try_send(buf, to);
                    let iface = bound.inner().iface().clone();
                    (ret, iface)
                }
                _ => return Err(SystemError::ENOTCONN),
            }
        }; // Lock released here

        // Poll AFTER releasing the lock to avoid deadlock
        // when socket sends to itself on loopback
        iface.poll();

        // For loopback packets, we need to wake up the polling thread to ensure timely delivery
        // The polling thread processes packets from TX -> loopback -> RX
        if let Ok(_) = &result {
            let is_loopback = if let Some(to_endpoint) = to {
                // Check if destination is loopback (127.0.0.0/8)
                matches!(to_endpoint.addr, smoltcp::wire::IpAddress::Ipv4(addr) if addr.octets()[0] == 127)
            } else {
                // Connected socket - check if remote is loopback
                if let Some(inner_read) = self.inner.try_read() {
                    if let Some(UdpInner::Bound(bound)) = inner_read.as_ref() {
                        if let Ok(remote) = bound.remote_endpoint() {
                            matches!(remote.addr, smoltcp::wire::IpAddress::Ipv4(addr) if addr.octets()[0] == 127)
                        } else {
                            false
                        }
                    } else {
                        false
                    }
                } else {
                    false
                }
            };

            if is_loopback {
                // Wake up the network polling thread to process loopback packets immediately
                // This ensures packets are delivered from TX -> loopback -> RX without delay
                self.netns().wakeup_poll_thread();
            }
        }

        result
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }
}

impl Socket for UdpSocket {
    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblock.store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(local_endpoint) = local_endpoint {
            return self.do_bind(local_endpoint);
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn send_buffer_size(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity())
            }
            _ => inner::DEFAULT_TX_BUF_SIZE,
        }
    }

    fn recv_buffer_size(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_recv_capacity())
            }
            _ => inner::DEFAULT_RX_BUF_SIZE,
        }
    }

    fn recv_bytes_available(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                // For UDP, FIONREAD should return the size of the first packet,
                // not the total bytes in the queue
                bound.with_mut_socket(|socket| {
                    match socket.peek() {
                        Ok((payload, _)) => payload.len(),
                        Err(_) => 0, // No packets available
                    }
                })
            }
            _ => 0,
        }
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        match endpoint {
            Endpoint::Ip(remote) => {
                // Port 0 is treated as disconnect (like AF_UNSPEC)
                // This matches Linux behavior where connect() to port 0 succeeds but disconnects the socket
                if remote.port == 0 {
                    log::debug!("UDP connect: port 0 treated as disconnect");
                    // Disconnect logic - same as AF_UNSPEC case
                    let should_unbind = {
                        match self.inner.read().as_ref() {
                            Some(UdpInner::Bound(inner)) => {
                                inner.disconnect();
                                inner.should_unbind_on_disconnect()
                            }
                            Some(UdpInner::Unbound(_)) => return Ok(()), // Already disconnected
                            None => return Err(SystemError::EBADF),
                        }
                    };

                    if should_unbind {
                        // Socket was implicitly bound by connect, unbind it
                        let mut inner_guard = self.inner.write();
                        if let Some(UdpInner::Bound(bound)) = inner_guard.take() {
                            bound.close();
                            inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                        }
                    }
                    return Ok(());
                }

                if !self.is_bound() {
                    self.bind_ephemeral(remote.addr)?;
                }
                match self.inner.read().as_ref() {
                    Some(UdpInner::Bound(inner)) => {
                        inner.connect(remote);
                        Ok(())
                    }
                    Some(_) => Err(SystemError::ENOTCONN),
                    None => Err(SystemError::EBADF),
                }
            }
            Endpoint::Unspecified => {
                // AF_UNSPEC: disconnect the UDP socket (clear remote endpoint)
                // If socket was implicitly bound (by connect), unbind it
                let should_unbind = {
                    match self.inner.read().as_ref() {
                        Some(UdpInner::Bound(inner)) => {
                            inner.disconnect();
                            inner.should_unbind_on_disconnect()
                        }
                        Some(UdpInner::Unbound(_)) => return Ok(()), // Already disconnected
                        None => return Err(SystemError::EBADF),
                    }
                };

                if should_unbind {
                    // Socket was implicitly bound by connect, unbind it
                    let mut inner_guard = self.inner.write();
                    if let Some(UdpInner::Bound(bound)) = inner_guard.take() {
                        bound.close();
                        inner_guard.replace(UdpInner::Unbound(UnboundUdp::new()));
                    }
                }
                Ok(())
            }
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if buffer.len() == 0 {
            log::info!("UDP send() called with ZERO-LENGTH buffer");
        }

        // Check if write is shutdown (0x02 = SEND_SHUTDOWN)
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        if shutdown_bits & 0x02 != 0 {
            return Err(SystemError::EPIPE);
        }

        if flags.contains(PMSG::DONTWAIT) {
            log::warn!("Nonblock send is not implemented yet");
        }

        return self.try_send(buffer, None);
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        // Check if write is shutdown (0x02 = SEND_SHUTDOWN)
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        if shutdown_bits & 0x02 != 0 {
            return Err(SystemError::EPIPE);
        }

        if flags.contains(PMSG::DONTWAIT) {
            log::warn!("Nonblock send is not implemented yet");
        }

        if let Endpoint::Ip(remote) = address {
            return self.try_send(buffer, Some(remote));
        }

        return Err(SystemError::EINVAL);
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        // Check if read is shutdown
        // Linux allows reading buffered data even after SHUT_RD, only returns EOF when buffer is empty
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let is_recv_shutdown = shutdown_bits & 0x01 != 0;

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            let result = self.try_recv(buffer);
            // If shutdown and no data available, return EOF instead of EWOULDBLOCK
            if is_recv_shutdown && matches!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)) {
                return Ok(0);
            }
            return result.map(|(len, _)| len);
        } else {
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                match self.try_recv(buffer) {
                    Ok((len, _endpoint)) => {
                        return Ok(len);
                    }
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        // If shutdown and no data available, return EOF
                        if is_recv_shutdown {
                            return Ok(0);
                        }
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // Linux allows reading buffered data even after SHUT_RD
        // Only return EOF when buffer is empty
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let is_recv_shutdown = shutdown_bits & 0x01 != 0;

        // could block io
        if let Some(endpoint) = address {
            self.connect(endpoint)?;
        }

        return if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            let result = self.try_recv(buffer);
            // If shutdown and no data available, return EOF
            if is_recv_shutdown && matches!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)) {
                // If connected, we can return (0, remote_endpoint)
                if let Some(UdpInner::Bound(bound)) = self.inner.read().as_ref() {
                    if let Ok(remote) = bound.remote_endpoint() {
                        return Ok((0, Endpoint::Ip(remote)));
                    }
                }
                // Not connected, can't provide endpoint, return error
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
            result.map(|(len, endpoint)| (len, Endpoint::Ip(endpoint)))
        } else {
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                match self.try_recv(buffer) {
                    Ok((len, endpoint)) => {
                        return Ok((len, Endpoint::Ip(endpoint)));
                    }
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        // If shutdown and no data available, return EOF
                        if is_recv_shutdown {
                            // If connected, return EOF with remote endpoint
                            if let Some(UdpInner::Bound(bound)) = self.inner.read().as_ref() {
                                if let Ok(remote) = bound.remote_endpoint() {
                                    return Ok((0, Endpoint::Ip(remote)));
                                }
                            }
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        wq_wait_event_interruptible!(self.wait_queue, self.can_recv(), {})?;
                        // log::debug!("UdpSocket::recv_from: wake up");
                    }
                    Err(e) => return Err(e),
                }
            }
        };
    }

    fn do_close(&self) -> Result<(), SystemError> {
        self.close();
        Ok(())
    }

    fn shutdown(&self, how: ShutdownBit) -> Result<(), SystemError> {
        // For UDP, shutdown requires the socket to be connected (both SHUT_RD and SHUT_WR)
        // Check if socket is connected
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                if bound.remote_endpoint().is_err() {
                    return Err(SystemError::ENOTCONN);
                }
            }
            Some(UdpInner::Unbound(_)) => {
                return Err(SystemError::ENOTCONN);
            }
            None => return Err(SystemError::EBADF),
        }

        // Set the shutdown bits atomically
        // Use fetch_or to set the bits we want
        let old = self.shutdown.fetch_or(
            (if how.is_recv_shutdown() { 0x01 } else { 0 })
                | (if how.is_send_shutdown() { 0x02 } else { 0 }),
            Ordering::Release,
        );

        log::debug!(
            "UDP shutdown: old={:#x}, recv={}, send={}",
            old,
            how.is_recv_shutdown(),
            how.is_send_shutdown()
        );

        // Wake up any threads blocked in recv() or send() so they can check the shutdown state
        self.wait_queue.wakeup_all(None);

        Ok(())
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        if level == PSOL::SOCKET {
            let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
            match opt {
                PSO::SNDBUF => {
                    // Set send buffer size
                    if val.len() < core::mem::size_of::<u32>() {
                        return Err(SystemError::EINVAL);
                    }
                    let size = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]) as usize;
                    // Linux doubles the requested size and enforces a minimum
                    // We'll store the requested size and let smoltcp use it when creating sockets
                    self.send_buf_size.store(size, Ordering::Release);
                    log::debug!("UDP setsockopt SO_SNDBUF: {}", size);
                    return Ok(());
                }
                PSO::RCVBUF => {
                    // Set receive buffer size
                    if val.len() < core::mem::size_of::<u32>() {
                        return Err(SystemError::EINVAL);
                    }
                    let size = u32::from_ne_bytes([val[0], val[1], val[2], val[3]]) as usize;
                    // Linux doubles the requested size and enforces a minimum
                    // We'll store the requested size and let smoltcp use it when creating sockets
                    self.recv_buf_size.store(size, Ordering::Release);
                    log::debug!("UDP setsockopt SO_RCVBUF: {}", size);
                    return Ok(());
                }
                _ => {
                    return Err(SystemError::ENOPROTOOPT);
                }
            }
        }
        Err(SystemError::ENOPROTOOPT)
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        if level == PSOL::SOCKET {
            let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
            match opt {
                PSO::SNDBUF => {
                    if value.len() < core::mem::size_of::<u32>() {
                        return Err(SystemError::EINVAL);
                    }
                    let size = self.send_buf_size.load(Ordering::Acquire);
                    // Linux doubles the value when returning it
                    // If 0 (not set), return default size
                    let actual_size = if size == 0 {
                        DEFAULT_TX_BUF_SIZE * 2
                    } else {
                        size * 2
                    };
                    let bytes = (actual_size as u32).to_ne_bytes();
                    value[0..4].copy_from_slice(&bytes);
                    return Ok(core::mem::size_of::<u32>());
                }
                PSO::RCVBUF => {
                    if value.len() < core::mem::size_of::<u32>() {
                        return Err(SystemError::EINVAL);
                    }
                    let size = self.recv_buf_size.load(Ordering::Acquire);
                    // Linux doubles the value when returning it
                    // If 0 (not set), return default size
                    let actual_size = if size == 0 {
                        DEFAULT_RX_BUF_SIZE * 2
                    } else {
                        size * 2
                    };
                    let bytes = (actual_size as u32).to_ne_bytes();
                    value[0..4].copy_from_slice(&bytes);
                    return Ok(core::mem::size_of::<u32>());
                }
                _ => {
                    return Err(SystemError::ENOPROTOOPT);
                }
            }
        }
        Err(SystemError::ENOPROTOOPT)
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => Ok(Endpoint::Ip(bound.remote_endpoint()?)),
            Some(_) => Err(SystemError::ENOTCONN),
            None => Err(SystemError::EBADF),
        }
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        use smoltcp::wire::{IpAddress::*, IpEndpoint, IpListenEndpoint};
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                let IpListenEndpoint { addr, port } = bound.endpoint();

                // If bound to "any" address (0.0.0.0 or ::), but connected to a specific address,
                // return the actual local address that would be used for the connection
                let local_addr = if addr.is_none() {
                    // Check if socket is connected
                    if let Ok(remote) = bound.remote_endpoint() {
                        // Use the same address type as the remote
                        // For loopback connections, use loopback address
                        match remote.addr {
                            Ipv4(ipv4) if ipv4.is_loopback() => Ipv4([127, 0, 0, 1].into()),
                            Ipv6(ipv6) if ipv6.is_loopback() => Ipv6([0, 0, 0, 0, 0, 0, 0, 1].into()),
                            Ipv4(_) => Ipv4([0, 0, 0, 0].into()), // Still return "any" for non-loopback
                            Ipv6(_) => Ipv6([0, 0, 0, 0, 0, 0, 0, 0].into()),
                        }
                    } else {
                        // Not connected, return "any"
                        Ipv4([0, 0, 0, 0].into())
                    }
                } else {
                    addr.unwrap()
                };

                Ok(Endpoint::Ip(IpEndpoint::new(local_addr, port)))
            }
            Some(_) => Ok(Endpoint::Ip(IpEndpoint::new(Ipv4([0, 0, 0, 0].into()), 0))),
            None => Err(SystemError::EBADF),
        }
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        use crate::filesystem::vfs::iov::IoVecs;
        use crate::net::posix::SockAddr;

        // Check for MSG_ERRQUEUE - we don't support error queues yet
        if flags.contains(PMSG::ERRQUEUE) {
            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
        }

        // Validate and create iovecs
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true);

        // Receive data from socket
        let (recv_size, src_endpoint) = self.recv_from(&mut buf, flags, None)?;

        // Scatter received data to user iovecs
        iovs.scatter(&buf[..recv_size])?;

        // Write source address if requested
        if !msg.msg_name.is_null() {
            let src_addr = msg.msg_name as *mut SockAddr;
            src_endpoint.write_to_user(src_addr, &mut msg.msg_namelen)?;
        } else {
            msg.msg_namelen = 0;
        }

        // No control messages for now
        msg.msg_controllen = 0;
        msg.msg_flags = 0;

        Ok(recv_size)
    }

    fn send_msg(
        &self,
        msg: &crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        use crate::filesystem::vfs::iov::IoVecs;
        use crate::net::posix::SockAddr;

        // Validate and gather iovecs
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, false)? };
        let data = iovs.gather()?;

        // Check if destination address is provided
        if !msg.msg_name.is_null() && msg.msg_namelen > 0 {
            // Send to specific address
            let endpoint = SockAddr::to_endpoint(msg.msg_name as *const SockAddr, msg.msg_namelen)?;
            self.send_to(&data, flags, endpoint)
        } else {
            // Send using connected endpoint
            self.send(&data, flags)
        }
    }

    fn epoll_items(&self) -> &crate::net::socket::common::EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();
        match self.inner.read().as_ref() {
            Some(UdpInner::Unbound(_)) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            Some(UdpInner::Bound(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| {
                        (socket.can_recv(), socket.can_send())
                    });

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
            None => {
                // Socket is closed
                event.insert(EP::EPOLLERR | EP::EPOLLHUP);
            }
        }
        event
    }

    fn socket_inode_id(&self) -> InodeId {
        self.inode_id
    }
}

impl InetSocket for UdpSocket {
    fn on_iface_events(&self) {
        // Wake up any threads waiting on this socket
        self.wait_queue.wakeup_all(None);

        // Notify epoll/poll watchers about socket state changes
        let pollflag = self.check_io_event();
        use crate::filesystem::epoll::event_poll::EventPoll;
        let _ = EventPoll::wakeup_epoll(self.epoll_items().as_ref(), pollflag);
    }
}

bitflags! {
    pub struct UdpSocketOptions: u32 {
        const ZERO = 0;        /* No UDP options */
        const UDP_CORK = 1;         /* Never send partially complete segments */
        const UDP_ENCAP = 100;      /* Set the socket to accept encapsulated packets */
        const UDP_NO_CHECK6_TX = 101; /* Disable sending checksum for UDP6X */
        const UDP_NO_CHECK6_RX = 102; /* Disable accepting checksum for UDP6 */
        const UDP_SEGMENT = 103;    /* Set GSO segmentation size */
        const UDP_GRO = 104;        /* This socket can receive UDP GRO packets */

        const UDPLITE_SEND_CSCOV = 10; /* sender partial coverage (as sent)      */
        const UDPLITE_RECV_CSCOV = 11; /* receiver partial coverage (threshold ) */
    }
}

bitflags! {
    pub struct UdpEncapTypes: u8 {
        const ZERO = 0;
        const ESPINUDP_NON_IKE = 1;     // draft-ietf-ipsec-nat-t-ike-00/01
        const ESPINUDP = 2;             // draft-ietf-ipsec-udp-encaps-06
        const L2TPINUDP = 3;            // rfc2661
        const GTP0 = 4;                 // GSM TS 09.60
        const GTP1U = 5;                // 3GPP TS 29.060
        const RXRPC = 6;
        const ESPINTCP = 7;             // Yikes, this is really xfrm encap types.
    }
}
