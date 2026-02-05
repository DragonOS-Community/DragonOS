use alloc::sync::Arc;

use smoltcp;
use system_error::SystemError;

use crate::{
    libs::mutex::Mutex, net::socket::inet::common::BoundInner,
    process::namespace::net_namespace::NetNamespace,
};

pub type SmolUdpSocket = smoltcp::socket::udp::Socket<'static>;

pub const DEFAULT_METADATA_BUF_SIZE: usize = 1024;
// UDP maximum datagram size is 65507 bytes (65535 - 8 byte UDP header - 20 byte IP header)
// Set buffer sizes to accommodate this plus some overhead
pub const DEFAULT_RX_BUF_SIZE: usize = 128 * 1024; // 128 KB
pub const DEFAULT_TX_BUF_SIZE: usize = 128 * 1024; // 128 KB
                                                   // Minimum buffer size (Linux uses 256 bytes minimum)

#[derive(Debug)]
pub struct UnboundUdp {
    socket: SmolUdpSocket,
}

impl UnboundUdp {
    pub fn new() -> Self {
        Self::new_with_buf_size(0, 0)
    }

    pub fn new_with_buf_size(rx_size: usize, tx_size: usize) -> Self {
        // Buffer sizing strategy:
        // - setsockopt(SO_RCVBUF, X) stores X
        // - getsockopt(SO_RCVBUF) returns 2*X (Linux convention)
        // - Actual buffer allocation: 2*X
        //
        // This is a straightforward 2x design that matches the getsockopt return value.
        //
        // Note: smoltcp's PacketBuffer has separate metadata_ring and payload_ring.
        // Unlike Linux where sk_buff metadata shares the same buffer space as payload,
        // smoltcp allocates them independently. This means:
        // - We allocate 2*X bytes purely for payload (no metadata overhead)
        // - This may accept more packets than Linux in some edge cases
        //
        // Differences from Linux behavior:
        // - Linux: Buffer space shared between metadata + payload, so effective payload < 2*X
        // - DragonOS: Full 2*X available for payload (metadata stored separately)

        let rx_buf_size = if rx_size > 0 {
            rx_size * 2 // Simple 2x allocation
        } else {
            DEFAULT_RX_BUF_SIZE
        };
        let tx_buf_size = if tx_size > 0 {
            tx_size * 2 // Simple 2x allocation
        } else {
            DEFAULT_TX_BUF_SIZE
        };

        // log::debug!(
        //     "new_with_buf_size: requested rx={}, tx={} -> allocating rx={}, tx={} (2x)",
        //     rx_size,
        //     tx_size,
        //     rx_buf_size,
        //     tx_buf_size
        // );

        let rx_buffer = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; rx_buf_size],
        );
        let tx_buffer = smoltcp::socket::udp::PacketBuffer::new(
            vec![smoltcp::socket::udp::PacketMetadata::EMPTY; DEFAULT_METADATA_BUF_SIZE],
            vec![0; tx_buf_size],
        );
        let socket = SmolUdpSocket::new(rx_buffer, tx_buffer);

        return Self { socket };
    }

    pub fn bind(
        self,
        local_endpoint: smoltcp::wire::IpEndpoint,
        netns: Arc<NetNamespace>,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
    ) -> Result<BoundUdp, SystemError> {
        let inner = BoundInner::bind(self.socket, &local_endpoint.addr, netns)?;
        let bind_addr = local_endpoint.addr;
        let bind_port = if local_endpoint.port == 0 {
            let port = inner
                .port_manager()
                .bind_udp_ephemeral_port(bind_addr, reuseaddr, reuseport, bind_id)?;
            // log::debug!("UnboundUdp::bind: allocated ephemeral port {}", port);
            port
        } else {
            inner.port_manager().bind_udp_port(
                local_endpoint.port,
                bind_addr,
                reuseaddr,
                reuseport,
                bind_id,
            )?;
            // log::debug!(
            //     "UnboundUdp::bind: explicit bind to port {}",
            //     local_endpoint.port
            // );
            local_endpoint.port
        };

        if bind_addr.is_unspecified() {
            if inner
                .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| socket.bind(bind_port))
                .is_err()
            {
                inner.port_manager().unbind_udp_port(bind_port, bind_id);
                return Err(SystemError::EINVAL);
            }
        } else if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(bind_addr, bind_port))
            })
            .is_err()
        {
            inner.port_manager().unbind_udp_port(bind_port, bind_id);
            return Err(SystemError::EINVAL);
        }
        let port_mgr_ifindex = inner.iface().nic_id();
        Ok(BoundUdp {
            inner,
            remote: Mutex::new(None),
            explicitly_bound: true,
            has_preconnect_data: Mutex::new(false),
            bind_id,
            port_mgr_ifindex,
        })
    }

    pub fn bind_ephemeral(
        self,
        remote: smoltcp::wire::IpAddress,
        netns: Arc<NetNamespace>,
        reuseaddr: bool,
        reuseport: bool,
        bind_id: usize,
    ) -> Result<BoundUdp, SystemError> {
        let (inner, local_addr) = BoundInner::bind_ephemeral(self.socket, remote, netns)?;
        let bound_port = inner
            .port_manager()
            .bind_udp_ephemeral_port(local_addr, reuseaddr, reuseport, bind_id)?;
        // log::debug!(
        //     "UnboundUdp::bind_ephemeral: allocated ephemeral port {} for remote {:?}",
        //     bound_port,
        //     remote
        // );

        // Bind the smoltcp socket to the local endpoint
        if local_addr.is_unspecified() {
            if inner
                .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| socket.bind(bound_port))
                .is_err()
            {
                inner.port_manager().unbind_udp_port(bound_port, bind_id);
                return Err(SystemError::EINVAL);
            }
        } else if inner
            .with_mut::<smoltcp::socket::udp::Socket, _, _>(|socket| {
                socket.bind(smoltcp::wire::IpEndpoint::new(local_addr, bound_port))
            })
            .is_err()
        {
            inner.port_manager().unbind_udp_port(bound_port, bind_id);
            return Err(SystemError::EINVAL);
        }

        let port_mgr_ifindex = inner.iface().nic_id();
        Ok(BoundUdp {
            inner,
            remote: Mutex::new(None),
            explicitly_bound: false,
            has_preconnect_data: Mutex::new(false),
            bind_id,
            port_mgr_ifindex,
        })
    }
}

#[derive(Debug)]
pub struct BoundUdp {
    inner: BoundInner,
    remote: Mutex<Option<smoltcp::wire::IpEndpoint>>,
    /// True if socket was explicitly bound by user, false if implicitly bound by connect
    explicitly_bound: bool,
    /// Whether there were buffered packets at connect time - if true, allow next recv without filtering
    /// 这是用来模拟 Linux UDP 在应用filter前的行为。在smoltcp下，当有包到来时总是会推送到
    /// udp socket queue 中，而不是先针对connect进行filter操作。这里做workaround, 当connect是检查是否有包
    /// 在缓冲区，如果有，第一个包我们走非connect而不是connect的recv方法（即接受第一个非connect对端对应的包）
    has_preconnect_data: Mutex<bool>,
    bind_id: usize,
    port_mgr_ifindex: usize,
}

impl BoundUdp {
    pub fn with_mut_socket<F, T>(&self, f: F) -> T
    where
        F: FnMut(&mut SmolUdpSocket) -> T,
    {
        self.inner.with_mut(f)
    }

    pub fn with_socket<F, T>(&self, f: F) -> T
    where
        F: Fn(&SmolUdpSocket) -> T,
    {
        self.inner.with(f)
    }

    pub fn endpoint(&self) -> smoltcp::wire::IpListenEndpoint {
        self.inner
            .with::<SmolUdpSocket, _, _>(|socket| socket.endpoint())
    }

    pub fn remote_endpoint(&self) -> Result<smoltcp::wire::IpEndpoint, SystemError> {
        self.remote
            .lock()
            .as_ref()
            .cloned()
            .ok_or(SystemError::ENOTCONN)
    }

    pub fn connect(&self, remote: smoltcp::wire::IpEndpoint) {
        // let _local = self.endpoint();
        // log::debug!(
        //     "BoundUdp::connect: local={:?}, connecting to remote={:?}",
        //     _local,
        //     remote
        // );

        // Check if there are buffered packets - if so, allow next recv without filtering
        let has_buffered = self.with_socket(|socket| socket.can_recv());
        *self.has_preconnect_data.lock() = has_buffered;
        // log::debug!("BoundUdp::connect: has pre-connect data = {}", has_buffered);

        self.remote.lock().replace(remote);
    }

    pub fn set_preconnect_data(&self, has_data: bool) {
        *self.has_preconnect_data.lock() = has_data;
    }

    pub fn has_preconnect_data(&self) -> bool {
        *self.has_preconnect_data.lock()
    }

    pub fn take_preconnect_data(&self) -> bool {
        let mut guard = self.has_preconnect_data.lock();
        let v = *guard;
        if v {
            *guard = false;
        }
        v
    }

    pub fn disconnect(&self) {
        self.remote.lock().take();
    }

    /// Returns true if this socket should be unbound on disconnect
    pub fn should_unbind_on_disconnect(&self) -> bool {
        !self.explicitly_bound
    }

    #[inline]
    pub fn try_recv(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<(usize, smoltcp::wire::IpEndpoint, usize), SystemError> {
        let remote = *self.remote.lock();

        self.with_mut_socket(|socket| {
            let endpoint_addr = socket.endpoint().addr;
            // If connected, filter packets by source address (except pre-connect packets)

            let mut has_preconnect_guard = self.has_preconnect_data.lock();
            let has_preconnect = *has_preconnect_guard;
            // let has_preconnect = false;
            if has_preconnect {
                *has_preconnect_guard = false;
            }
            drop(has_preconnect_guard);
            let should_filter = remote.is_some() && !has_preconnect;
            if should_filter {
                let expected_remote = remote.unwrap();
                // log::debug!("try_recv: connected mode, expected_remote={:?}, buf_len={}, can_recv={}",
                //     expected_remote, buf.len(), socket.can_recv());

                // Loop to skip packets from unexpected sources
                loop {
                    if !socket.can_recv() {
                        // log::debug!("try_recv: can_recv=false, returning EAGAIN");
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }

                    // Peek to check source address before receiving
                    // Note: peek() instead of peek_slice() because peek_slice() returns Truncated
                    // error when buffer is smaller than packet, but we still want to receive it
                    match socket.peek() {
                        Ok((payload, metadata)) => {
                            if let (Some(bound), Some(dst)) =
                                (endpoint_addr, metadata.local_address)
                            {
                                if bound != dst
                                    && (dst.is_multicast()
                                        || dst.is_broadcast()
                                        || bound.is_multicast()
                                        || bound.is_broadcast())
                                {
                                    let _ = socket.recv();
                                    continue;
                                }
                            }
                            // log::debug!("try_recv: peeked {} bytes from {:?}, buf_len={}", payload.len(), metadata.endpoint, buf.len());
                            if metadata.endpoint == expected_remote {
                                // Source matches

                                // Special case: zero-length buffer
                                if buf.is_empty() {
                                    // log::debug!("try_recv: zero-length buffer in connected mode, returning 0 bytes");
                                    return Ok((0, expected_remote, payload.len()));
                                }

                                if peek {
                                    // MSG_PEEK: just copy the data we peeked
                                    let copy_len = core::cmp::min(buf.len(), payload.len());
                                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                                    // log::debug!("try_recv: peek succeeded, size={}", copy_len);
                                    return Ok((copy_len, expected_remote, payload.len()));
                                } else {
                                    // Receive the packet
                                    let (recv_buf, _metadata) =
                                        socket.recv().map_err(|_| SystemError::ENOBUFS)?;
                                    let length = core::cmp::min(buf.len(), recv_buf.len());
                                    buf[..length].copy_from_slice(&recv_buf[..length]);
                                    debug_assert_eq!(expected_remote, _metadata.endpoint);
                                    return Ok((length, expected_remote, recv_buf.len()));
                                }
                            } else {
                                // just drop the packet
                                let _ = socket.recv();
                                continue;
                            }
                        }
                        Err(smoltcp::socket::udp::RecvError::Exhausted) => {
                            return Err(SystemError::ENOBUFS)
                        }
                        Err(_e) => return Err(SystemError::EIO),
                    }
                }
            } else {
                // log::debug!("try_recv: unconnected mode, buf_len={}, can_recv={}", buf.len(), socket.can_recv());
                // Not connected, receive from any source

                // Special case: if buffer length is 0, just peek to check if data exists
                if buf.is_empty() {
                    loop {
                        if !socket.can_recv() {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        match socket.peek() {
                            Ok((payload, metadata)) => {
                                if let (Some(bound), Some(dst)) =
                                    (endpoint_addr, metadata.local_address)
                                {
                                    if bound != dst
                                        && (dst.is_multicast()
                                            || dst.is_broadcast()
                                            || bound.is_multicast()
                                            || bound.is_broadcast())
                                    {
                                        let _ = socket.recv();
                                        continue;
                                    }
                                }
                                return Ok((0, metadata.endpoint, payload.len()));
                            }
                            Err(smoltcp::socket::udp::RecvError::Exhausted) => {
                                return Err(SystemError::ENOBUFS)
                            }
                            Err(_e) => return Err(SystemError::EIO),
                        }
                    }
                }

                loop {
                    if !socket.can_recv() {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    match socket.peek() {
                        Ok((payload, metadata)) => {
                            if let (Some(bound), Some(dst)) =
                                (endpoint_addr, metadata.local_address)
                            {
                                if bound != dst
                                    && (dst.is_multicast()
                                        || dst.is_broadcast()
                                        || bound.is_multicast()
                                        || bound.is_broadcast())
                                {
                                    let _ = socket.recv();
                                    continue;
                                }
                            }
                            if peek {
                                let copy_len = core::cmp::min(buf.len(), payload.len());
                                buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                                return Ok((copy_len, metadata.endpoint, payload.len()));
                            } else {
                                let (recv_buf, recv_meta) =
                                    socket.recv().map_err(|_| SystemError::ENOBUFS)?;
                                let length = core::cmp::min(buf.len(), recv_buf.len());
                                buf[..length].copy_from_slice(&recv_buf[..length]);
                                return Ok((length, recv_meta.endpoint, recv_buf.len()));
                            }
                        }
                        Err(smoltcp::socket::udp::RecvError::Exhausted) => {
                            return Err(SystemError::ENOBUFS)
                        }
                        Err(_e) => return Err(SystemError::EIO),
                    }
                }
            }
        })
    }

    pub fn try_recv_with_metadata(
        &self,
        buf: &mut [u8],
        peek: bool,
    ) -> Result<
        (
            usize,
            smoltcp::wire::IpEndpoint,
            usize,
            Option<smoltcp::wire::IpAddress>,
        ),
        SystemError,
    > {
        let remote = *self.remote.lock();

        self.with_mut_socket(|socket| {
            let endpoint_addr = socket.endpoint().addr;
            let mut has_preconnect_guard = self.has_preconnect_data.lock();
            let has_preconnect = *has_preconnect_guard;
            if has_preconnect {
                *has_preconnect_guard = false;
            }
            drop(has_preconnect_guard);
            let should_filter = remote.is_some() && !has_preconnect;

            if should_filter {
                let expected_remote = remote.unwrap();
                loop {
                    if !socket.can_recv() {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    match socket.peek() {
                        Ok((payload, metadata)) => {
                            if let (Some(bound), Some(dst)) =
                                (endpoint_addr, metadata.local_address)
                            {
                                if bound != dst
                                    && (dst.is_multicast()
                                        || dst.is_broadcast()
                                        || bound.is_multicast()
                                        || bound.is_broadcast())
                                {
                                    let _ = socket.recv();
                                    continue;
                                }
                            }
                            if metadata.endpoint == expected_remote {
                                if buf.is_empty() {
                                    return Ok((
                                        0,
                                        expected_remote,
                                        payload.len(),
                                        metadata.local_address,
                                    ));
                                }
                                if peek {
                                    let copy_len = core::cmp::min(buf.len(), payload.len());
                                    buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                                    return Ok((
                                        copy_len,
                                        expected_remote,
                                        payload.len(),
                                        metadata.local_address,
                                    ));
                                } else {
                                    let (recv_buf, recv_meta) =
                                        socket.recv().map_err(|_| SystemError::ENOBUFS)?;
                                    let length = core::cmp::min(buf.len(), recv_buf.len());
                                    buf[..length].copy_from_slice(&recv_buf[..length]);
                                    debug_assert_eq!(expected_remote, recv_meta.endpoint);
                                    return Ok((
                                        length,
                                        expected_remote,
                                        recv_buf.len(),
                                        recv_meta.local_address,
                                    ));
                                }
                            } else {
                                let _ = socket.recv();
                                continue;
                            }
                        }
                        Err(smoltcp::socket::udp::RecvError::Exhausted) => {
                            return Err(SystemError::ENOBUFS)
                        }
                        Err(_e) => return Err(SystemError::EIO),
                    }
                }
            } else {
                if buf.is_empty() {
                    loop {
                        if !socket.can_recv() {
                            return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                        }
                        match socket.peek() {
                            Ok((payload, metadata)) => {
                                if let (Some(bound), Some(dst)) =
                                    (endpoint_addr, metadata.local_address)
                                {
                                    if bound != dst
                                        && (dst.is_multicast()
                                            || dst.is_broadcast()
                                            || bound.is_multicast()
                                            || bound.is_broadcast())
                                    {
                                        let _ = socket.recv();
                                        continue;
                                    }
                                }
                                return Ok((
                                    0,
                                    metadata.endpoint,
                                    payload.len(),
                                    metadata.local_address,
                                ));
                            }
                            Err(smoltcp::socket::udp::RecvError::Exhausted) => {
                                return Err(SystemError::ENOBUFS)
                            }
                            Err(_e) => return Err(SystemError::EIO),
                        }
                    }
                }

                loop {
                    if !socket.can_recv() {
                        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                    }
                    match socket.peek() {
                        Ok((payload, metadata)) => {
                            if let (Some(bound), Some(dst)) =
                                (endpoint_addr, metadata.local_address)
                            {
                                if bound != dst
                                    && (dst.is_multicast()
                                        || dst.is_broadcast()
                                        || bound.is_multicast()
                                        || bound.is_broadcast())
                                {
                                    let _ = socket.recv();
                                    continue;
                                }
                            }
                            if peek {
                                let copy_len = core::cmp::min(buf.len(), payload.len());
                                buf[..copy_len].copy_from_slice(&payload[..copy_len]);
                                return Ok((
                                    copy_len,
                                    metadata.endpoint,
                                    payload.len(),
                                    metadata.local_address,
                                ));
                            } else {
                                let (recv_buf, recv_meta) =
                                    socket.recv().map_err(|_| SystemError::ENOBUFS)?;
                                let length = core::cmp::min(buf.len(), recv_buf.len());
                                buf[..length].copy_from_slice(&recv_buf[..length]);
                                return Ok((
                                    length,
                                    recv_meta.endpoint,
                                    recv_buf.len(),
                                    recv_meta.local_address,
                                ));
                            }
                        }
                        Err(smoltcp::socket::udp::RecvError::Exhausted) => {
                            return Err(SystemError::ENOBUFS)
                        }
                        Err(_e) => return Err(SystemError::EIO),
                    }
                }
            }
        })
    }

    pub fn try_send(
        &self,
        buf: &[u8],
        to: Option<smoltcp::wire::IpEndpoint>,
    ) -> Result<usize, SystemError> {
        let connected_remote = *self.remote.lock();
        let mut remote = to.or(connected_remote).ok_or(SystemError::ENOTCONN)?;

        // Validate port - sending to port 0 is invalid
        if remote.port == 0 {
            log::warn!("UDP try_send: attempted to send to port 0");
            return Err(SystemError::EINVAL);
        }

        // Linux treats sending to 0.0.0.0 (INADDR_ANY) as sending to localhost
        // smoltcp rejects it as "Unaddressable", so we translate it here
        if remote.addr.is_unspecified() {
            remote.addr = smoltcp::wire::IpAddress::v4(127, 0, 0, 1);
        }

        // log::debug!(
        //     "try_send: sending {} bytes to {:?}, can_send={}",
        //     buf.len(),
        //     remote,
        //     self.with_socket(|socket| socket.can_send())
        // );

        self.with_mut_socket(|socket| {
            let max_payload = socket.payload_send_capacity();
            if buf.len() > max_payload || buf.len() > u16::MAX as usize {
                return Err(SystemError::EMSGSIZE);
            }
            if socket.can_send() {
                match socket.send_slice(buf, remote) {
                    Ok(_) => {
                        // log::debug!("try_send: send successful");
                        Ok(buf.len())
                    }
                    Err(_e) => {
                        // log::debug!("try_send: send failed: {:?}", _e);
                        Err(SystemError::ENOBUFS)
                    }
                }
            } else {
                // log::debug!("try_send: can_send=false, returning ENOBUFS");
                Err(SystemError::ENOBUFS)
            }
        })
    }

    pub fn inner(&self) -> &BoundInner {
        &self.inner
    }

    pub fn inner_mut(&mut self) -> &mut BoundInner {
        &mut self.inner
    }

    pub fn close(&self) {
        let netns = self.inner.netns();
        crate::net::socket::inet::common::multicast::find_iface_by_ifindex(
            &netns,
            self.port_mgr_ifindex as i32,
        )
        .unwrap_or_else(|| self.inner.iface().clone())
        .port_manager()
        .unbind_udp_port(self.endpoint().port, self.bind_id);
        self.with_mut_socket(|socket| {
            socket.close();
        });
    }
}

// Udp Inner 负责其内部资源管理
#[derive(Debug)]
pub enum UdpInner {
    Unbound(UnboundUdp),
    Bound(BoundUdp),
}
