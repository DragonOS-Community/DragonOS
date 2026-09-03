use super::*;

impl Socket for UdpSocket {
    fn netns(&self) -> Arc<crate::process::namespace::net_namespace::NetNamespace> {
        UdpSocket::netns(self)
    }

    fn fsnotify_watch_counter(&self) -> &AtomicUsize {
        &self.fsnotify_watches
    }

    fn open_file_counter(&self) -> &AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &WaitQueue {
        &self.wait_queue
    }

    fn set_nonblocking(&self, nonblocking: bool) {
        self.nonblock
            .store(nonblocking, core::sync::atomic::Ordering::Relaxed);
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        match local_endpoint {
            Endpoint::Ip(local_endpoint) => self.do_bind(local_endpoint),
            Endpoint::Unspecified => {
                // AF_UNSPEC on bind() is a no-op for AF_INET sockets (Linux compatibility)
                // See: https://github.com/torvalds/linux/commit/29c486df6a208432b370bd4be99ae1369ede28d8
                // log::debug!("UDP bind: AF_UNSPEC treated as no-op for compatibility");
                Ok(())
            }
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }

    fn send_buffer_size(&self) -> usize {
        // Check if custom buffer size was set via setsockopt
        let custom_size = self.send_buf_size.load(Ordering::Acquire);
        if custom_size > 0 {
            // Linux doubles the value when returning via getsockopt
            return custom_size * 2;
        }

        // Otherwise return actual buffer capacity
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity())
            }
            _ => inner::DEFAULT_TX_BUF_SIZE * 2, // Linux doubles default too
        }
    }

    fn recv_buffer_size(&self) -> usize {
        // Check if custom buffer size was set via setsockopt
        let custom_size = self.recv_buf_size.load(Ordering::Acquire);
        if custom_size > 0 {
            // Linux doubles the value when returning via getsockopt
            // log::debug!(
            //     "recv_buffer_size: custom_size={}, returning={}",
            //     custom_size,
            //     custom_size * 2
            // );
            return custom_size * 2;
        }

        // Otherwise return actual buffer capacity
        let size = match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_recv_capacity())
            }
            _ => inner::DEFAULT_RX_BUF_SIZE * 2, // Linux doubles default too
        };
        // log::debug!("recv_buffer_size: no custom size, returning={}", size);
        size
    }

    fn recv_bytes_available(&self) -> usize {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                // 优先检查 loopback 队列，返回第一条可接收报文的长度。
                let loopback_len = {
                    let loopback_rx = self.multicast_loopback_rx.lock();
                    loopback_rx
                        .iter()
                        .find(|pkt| self.loopback_accepts_with_preconnect(pkt, false))
                        .map(|pkt| pkt.payload.len())
                };
                if let Some(len) = loopback_len {
                    return len;
                }

                // For UDP, FIONREAD should return the size of the first packet,
                // not the total bytes in the queue
                bound.with_mut_socket(|socket| match socket.peek() {
                    Ok((payload, _)) => payload.len(),
                    Err(_) => 0, // No packets available
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
                    return self.disconnect_udp();
                }

                // The connected peer participates in send-side interface
                // selection, so keep it stable while readers classify a send.
                let _placement = self.iface_placement.write();
                let remote = Self::normalize_unspecified_dest(remote);
                if !self.is_bound() {
                    self.bind_ephemeral(remote.addr)?;
                }
                let local = match self.inner.read().as_ref() {
                    Some(UdpInner::Bound(inner)) => inner.endpoint(),
                    Some(_) => return Err(SystemError::ENOTCONN),
                    None => return Err(SystemError::EBADF),
                };
                let connected_source = output_flow::resolve_wildcard_ipv4(
                    &self.netns,
                    local,
                    remote.addr,
                    self.device_binding
                        .resolve_iface(&self.netns)?
                        .map(|iface| iface.nic_id() as u32),
                    None,
                    remote.addr.is_multicast(),
                    remote.addr.is_broadcast(),
                )?
                .map(|flow| flow.source);
                match self.inner.read().as_ref() {
                    Some(UdpInner::Bound(inner)) => {
                        inner.connect(remote, connected_source);
                        if !self.multicast_loopback_rx.lock().is_empty() {
                            inner.set_preconnect_data(true);
                        }
                        Ok(())
                    }
                    Some(_) => Err(SystemError::ENOTCONN),
                    None => Err(SystemError::EBADF),
                }
            }
            Endpoint::Unspecified => {
                // AF_UNSPEC disconnects and drops an implicit bind.
                self.disconnect_udp()
            }
            _ => Err(SystemError::EAFNOSUPPORT),
        }
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        if buffer.is_empty() {
            log::debug!("UDP send() called with ZERO-LENGTH buffer");
        }

        // Check if write is shutdown (0x02 = SEND_SHUTDOWN)
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        if shutdown_bits & 0x02 != 0 {
            return Err(SystemError::EPIPE);
        }

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            return self.try_send(buffer, None);
        } else {
            let deadline = self.send_timeout().map(|t| Instant::now() + t);
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                if shutdown_bits & 0x02 != 0 {
                    return Err(SystemError::EPIPE);
                }

                match self.try_send(buffer, None) {
                    Ok(len) => return Ok(len),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        let timeout = deadline
                            .map(|d| d.duration_since(Instant::now()).unwrap_or(Duration::ZERO));
                        self.wait_queue
                            .wait_event_io_interruptible_timeout(|| self.can_send(), timeout)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        // Check if write is shutdown (0x02 = SEND_SHUTDOWN)
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        if shutdown_bits & 0x02 != 0 {
            return Err(SystemError::EPIPE);
        }

        let remote = if let Endpoint::Ip(remote) = address {
            remote
        } else {
            return Err(SystemError::EINVAL);
        };

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            return self.try_send(buffer, Some(remote));
        } else {
            let deadline = self.send_timeout().map(|t| Instant::now() + t);
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                if shutdown_bits & 0x02 != 0 {
                    return Err(SystemError::EPIPE);
                }

                match self.try_send(buffer, Some(remote)) {
                    Ok(len) => return Ok(len),
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        let timeout = deadline
                            .map(|d| d.duration_since(Instant::now()).unwrap_or(Duration::ZERO));
                        self.wait_queue
                            .wait_event_io_interruptible_timeout(|| self.can_send(), timeout)?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn validate_send_buffer_len(
        &self,
        len: usize,
        address: Option<&Endpoint>,
    ) -> Result<(), SystemError> {
        if self.ip_version == IpVersion::Ipv6
            && matches!(address, Some(Endpoint::Ip(dest)) if dest.port == 0)
        {
            return Err(SystemError::EINVAL);
        }

        if len > u16::MAX as usize {
            let offender = self.connected_or_explicit_send_dest(address);
            self.enqueue_ipv6_emsgsize_errqueue(len, offender);
            return Err(SystemError::EMSGSIZE);
        }
        Ok(())
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        // Check if read is shutdown
        // Linux allows reading buffered data even after SHUT_RD, only returns EOF when buffer is empty
        let shutdown_bits = self.shutdown.load(Ordering::Acquire);
        let is_recv_shutdown = shutdown_bits & 0x01 != 0;

        let peek = flags.contains(PMSG::PEEK);

        if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            let result = self.try_recv(buffer, peek);
            // If shutdown and no data available, return EOF instead of EWOULDBLOCK
            if is_recv_shutdown && matches!(result, Err(SystemError::EAGAIN_OR_EWOULDBLOCK)) {
                return Ok(0);
            }
            return result.map(|(copy_len, _endpoint, orig_len)| {
                Self::recv_return_len(copy_len, orig_len, flags)
            });
        } else {
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                match self.try_recv(buffer, peek) {
                    Ok((copy_len, _endpoint, orig_len)) => {
                        return Ok(Self::recv_return_len(copy_len, orig_len, flags));
                    }
                    Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                        // If shutdown and no data available, return EOF
                        if is_recv_shutdown {
                            return Ok(0);
                        }
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.can_recv(),
                            self.recv_timeout(),
                        )?;
                    }
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn read_to_user_buffer(
        &self,
        user_buffer: &mut crate::syscall::user_buffer::UserBuffer<'_>,
    ) -> Result<usize, SystemError> {
        crate::net::socket::base::read_to_user_buffer_via_kernel_buf(
            self,
            user_buffer,
            self.recv_buffer_size(),
        )
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        _address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        // Linux allows reading buffered data even after SHUT_RD
        // For blocking mode, check shutdown state in the loop

        let peek = flags.contains(PMSG::PEEK);

        return if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
            let result = self.try_recv(buffer, peek);
            // For non-blocking sockets, always return EAGAIN when no data
            // Even after shutdown, don't convert to EOF
            result.map(|(copy_len, endpoint, orig_len)| {
                (
                    Self::recv_return_len(copy_len, orig_len, flags),
                    Endpoint::Ip(endpoint),
                )
            })
        } else {
            loop {
                // Re-check shutdown state inside the loop
                let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                match self.try_recv(buffer, peek) {
                    Ok((copy_len, endpoint, orig_len)) => {
                        return Ok((
                            Self::recv_return_len(copy_len, orig_len, flags),
                            Endpoint::Ip(endpoint),
                        ));
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
                        self.wait_queue.wait_event_io_interruptible_timeout(
                            || self.can_recv(),
                            self.recv_timeout(),
                        )?;
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
        let _old = self.shutdown.fetch_or(
            (if how.is_recv_shutdown() { 0x01 } else { 0 })
                | (if how.is_send_shutdown() { 0x02 } else { 0 }),
            Ordering::Release,
        );

        // log::debug!(
        //     "UDP shutdown: old={:#x}, recv={}, send={}",
        //     _old,
        //     how.is_recv_shutdown(),
        //     how.is_send_shutdown()
        // );

        // Wake up any threads blocked in recv() or send() so they can check the shutdown state
        self.wait_queue.wakeup_all(None);

        Ok(())
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_socket_option(opt, val)
            }
            PSOL::IP => {
                let opt = IpOption::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_ip_option(opt, val)
            }
            PSOL::IPV6 => {
                if self.ip_version != IpVersion::Ipv6 {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let opt = PIPV6::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.set_ipv6_option(opt, val)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        // log::debug!(
        //     "UDP getsockopt called: level={:?}, name={}, value_len={}",
        //     level,
        //     name,
        //     value.len()
        // );
        match level {
            PSOL::SOCKET => {
                let opt = PSO::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_socket_option(opt, value)
            }
            PSOL::IP => {
                let opt = IpOption::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_ip_option(opt, value)
            }
            PSOL::IPV6 => {
                if self.ip_version != IpVersion::Ipv6 {
                    return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
                }
                let opt = PIPV6::try_from(name as u32).map_err(|_| SystemError::ENOPROTOOPT)?;
                self.get_ipv6_option(opt, value)
            }
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => Ok(Endpoint::Ip(bound.remote_endpoint()?)),
            Some(_) => Err(SystemError::ENOTCONN),
            None => Err(SystemError::EBADF),
        }
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let unspecified_addr = match self.ip_version {
            IpVersion::Ipv4 => UNSPECIFIED_LOCAL_ENDPOINT_V4.addr,
            IpVersion::Ipv6 => UNSPECIFIED_LOCAL_ENDPOINT_V6.addr,
        };

        match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                let IpListenEndpoint { addr, port } = bound.endpoint();

                // If bound to "any" address (0.0.0.0 or ::), but connected to a specific address,
                // return the actual local address that would be used for the connection
                let local_addr = if let Some(addr) = addr {
                    addr
                } else if let Some(source) = bound.connected_source() {
                    source
                } else {
                    // Socket is bound to ANY - check if connected
                    if let Ok(remote) = bound.remote_endpoint() {
                        // Connected: return the local address for the interface that can reach the remote
                        // For loopback, return loopback address; otherwise get from interface
                        match remote.addr {
                            Ipv4(addr) if addr.is_loopback() => Ipv4(addr),
                            Ipv6(addr) if addr.is_loopback() => Ipv6(addr),
                            _ => {
                                // Get the first IP address from the interface
                                let iface_guard = bound.inner().iface().smol_iface().lock();
                                if let Some(cidr) = iface_guard.ip_addrs().first() {
                                    cidr.address()
                                } else {
                                    unspecified_addr
                                }
                            }
                        }
                    } else {
                        // Not connected, return "any"
                        unspecified_addr
                    }
                };

                Ok(Endpoint::Ip(IpEndpoint::new(local_addr, port)))
            }
            Some(_) => match self.ip_version {
                IpVersion::Ipv4 => Ok(Endpoint::Ip(UNSPECIFIED_LOCAL_ENDPOINT_V4)),
                IpVersion::Ipv6 => Ok(Endpoint::Ip(UNSPECIFIED_LOCAL_ENDPOINT_V6)),
            },
            None => Err(SystemError::EBADF),
        }
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        // log::debug!(
        //     "recv_msg: msg_name={:?}, msg_namelen={}, flags={:?}",
        //     msg.msg_name,
        //     msg.msg_namelen,
        //     flags
        // );

        // Handle MSG_ERRQUEUE for socket error queue
        if flags.contains(PMSG::ERRQUEUE) {
            let entry = self
                .pop_errqueue()
                .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;

            // Write offender address if requested
            let offender_ep = Endpoint::Ip(entry.offender);
            msg.msg_namelen = offender_ep.write_to_user_msghdr(msg.msg_name, msg.msg_namelen)?;

            // Prepare control message: sock_extended_err + offender sockaddr
            let err_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&entry.err as *const SockExtendedErr) as *const u8,
                    core::mem::size_of::<SockExtendedErr>(),
                )
            };
            let sockaddr = SockAddr::from(offender_ep);
            let sockaddr_bytes = unsafe {
                core::slice::from_raw_parts(
                    (&sockaddr as *const SockAddr) as *const u8,
                    entry.addr_len,
                )
            };

            let mut data = alloc::vec::Vec::with_capacity(err_bytes.len() + sockaddr_bytes.len());
            data.extend_from_slice(err_bytes);
            data.extend_from_slice(sockaddr_bytes);

            msg.msg_flags = PMSG::ERRQUEUE.bits() as i32;
            let mut write_off = 0usize;
            let mut cmsg_buf = CmsgBuffer {
                ptr: msg.msg_control,
                len: msg.msg_controllen,
                write_off: &mut write_off,
            };
            cmsg_buf.put(
                &mut msg.msg_flags,
                entry.cmsg_level,
                entry.cmsg_type,
                data.len(),
                &data,
            )?;
            msg.msg_controllen = write_off;

            return Ok(0);
        }

        // Validate and create iovecs
        let iovs = unsafe { IoVecs::from_user(msg.msg_iov, msg.msg_iovlen, true)? };
        let mut buf = iovs.new_buf(true)?;
        let buf_cap = buf.len();

        // Receive data from socket
        let (copy_len, src_endpoint, orig_len, dst_addr, ifindex) = {
            let peek = flags.contains(PMSG::PEEK);
            if self.is_nonblock() || flags.contains(PMSG::DONTWAIT) {
                let (copy_len, endpoint, orig_len, dst_addr, ifindex) =
                    self.try_recv_with_meta(&mut buf, peek)?;
                (
                    copy_len,
                    Endpoint::Ip(endpoint),
                    orig_len,
                    dst_addr,
                    ifindex,
                )
            } else {
                loop {
                    // Re-check shutdown state inside the loop
                    let shutdown_bits = self.shutdown.load(Ordering::Acquire);
                    let is_recv_shutdown = shutdown_bits & 0x01 != 0;

                    match self.try_recv_with_meta(&mut buf, peek) {
                        Ok((copy_len, endpoint, orig_len, dst_addr, ifindex)) => {
                            break (
                                copy_len,
                                Endpoint::Ip(endpoint),
                                orig_len,
                                dst_addr,
                                ifindex,
                            );
                        }
                        Err(SystemError::EAGAIN_OR_EWOULDBLOCK) => {
                            // If shutdown and no data available, return EOF
                            if is_recv_shutdown {
                                if let Some(UdpInner::Bound(bound)) = self.inner.read().as_ref() {
                                    if let Ok(remote) = bound.remote_endpoint() {
                                        break (
                                            0,
                                            Endpoint::Ip(remote),
                                            0,
                                            self.unspecified_addr(),
                                            0,
                                        );
                                    }
                                }
                                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
                            }
                            self.wait_queue.wait_event_io_interruptible_timeout(
                                || self.can_recv(),
                                self.recv_timeout(),
                            )?;
                        }
                        Err(e) => return Err(e),
                    }
                }
            }
        };

        // log::debug!(
        //     "recv_msg: received {} bytes from {:?}",
        //     recv_size,
        //     src_endpoint
        // );

        // Scatter received data to user iovecs
        iovs.scatter_exact(&buf[..copy_len])?;

        // Write source address if requested
        if !msg.msg_name.is_null() {
            let src_addr = msg.msg_name;
            // log::debug!(
            //     "recv_msg: writing endpoint to user, msg_namelen={}",
            //     msg.msg_namelen
            // );
            let actual_len = src_endpoint.write_to_user_msghdr(src_addr, msg.msg_namelen)?;
            msg.msg_namelen = actual_len;
            // log::debug!(
            //     "recv_msg: endpoint written, updated msg_namelen={}",
            //     msg.msg_namelen
            // );
        } else {
            // log::debug!("recv_msg: msg_name is NULL, skipping endpoint write");
            msg.msg_namelen = 0;
        }

        let cmsg_len = msg.msg_controllen;
        msg.msg_controllen = 0;
        msg.msg_flags = 0;
        if orig_len > buf_cap {
            msg.msg_flags |= PMSG::TRUNC.bits() as i32;
        }
        if cmsg_len > 0 {
            let mut write_off = 0usize;
            let mut cmsg_buf = CmsgBuffer {
                ptr: msg.msg_control,
                len: cmsg_len,
                write_off: &mut write_off,
            };
            self.build_udp_recv_cmsgs(&mut cmsg_buf, &mut msg.msg_flags, dst_addr, ifindex)?;
            msg.msg_controllen = write_off;
        }

        // log::debug!("recv_msg: returning {} bytes", recv_size);
        Ok(Self::recv_return_len(copy_len, orig_len, flags))
    }

    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        // Validate and gather iovecs
        // TODO: Actual iovecs sends
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
        let loopback_has_data = !self.multicast_loopback_rx.lock().is_empty();
        match self.inner.read().as_ref() {
            Some(UdpInner::Unbound(_)) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            Some(UdpInner::Bound(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv || loopback_has_data {
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

    fn send_bytes_available(&self) -> Result<usize, SystemError> {
        Ok(match self.inner.read().as_ref() {
            Some(UdpInner::Bound(bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity() - socket.send_queue())
            }
            _ => 0,
        })
    }
}

impl InetSocket for UdpSocket {
    fn on_iface_events(&self) {
        // Wake up any threads waiting on this socket
        self.wait_queue.wakeup_all(None);

        // Notify epoll/poll watchers about socket state changes
        let pollflag = self.check_io_event();
        let _ = EventPoll::wakeup_epoll(self.epoll_items().as_ref(), pollflag);
    }
}
