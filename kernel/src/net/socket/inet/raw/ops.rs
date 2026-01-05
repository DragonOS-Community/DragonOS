use alloc::sync::Arc;

use smoltcp::wire::IpAddress;
use system_error::SystemError;

use crate::filesystem::epoll::EPollEventType;
use crate::net::socket::endpoint::Endpoint;
use crate::net::socket::{PMSG, PSOL};

use super::inner::RawInner;
use super::{InetSocket, RawSocket};

type EP = crate::filesystem::epoll::EPollEventType;

impl crate::net::socket::Socket for RawSocket {
    fn open_file_counter(&self) -> &core::sync::atomic::AtomicUsize {
        &self.open_files
    }

    fn wait_queue(&self) -> &crate::libs::wait_queue::WaitQueue {
        &self.wait_queue
    }

    fn bind(&self, local_endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(endpoint) = local_endpoint {
            return self.do_bind(endpoint.addr);
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn send_buffer_size(&self) -> usize {
        self.options.read().sock_sndbuf as usize
    }

    fn recv_buffer_size(&self) -> usize {
        self.options.read().sock_rcvbuf as usize
    }

    fn connect(&self, endpoint: Endpoint) -> Result<(), SystemError> {
        if let Endpoint::Ip(remote) = endpoint {
            if !self.addr_matches_ip_version(remote.addr) {
                return Err(SystemError::EAFNOSUPPORT);
            }

            // Linux 语义：connect(2) 会为本端选择一个具体的可路由地址，
            // 使得 getsockname(2) 不返回 0.0.0.0/::。
            let need_local = match self.inner.read().as_ref() {
                Some(RawInner::Bound(b) | RawInner::Wildcard(b)) => b.local_addr().is_none(),
                _ => true,
            };

            // Linux 语义（对本测例）：connect(2) 后 getsockname 应返回可路由的本地地址。
            // 对 loopback 目标，直接绑定到 loopback 地址，避免返回 0.0.0.0/::。
            if need_local {
                let bind_local = match remote.addr {
                    IpAddress::Ipv4(v4) if v4.is_loopback() => Some(IpAddress::Ipv4(v4)),
                    IpAddress::Ipv6(v6) if v6.is_loopback() => Some(IpAddress::Ipv6(v6)),
                    _ => None,
                };
                if let Some(local) = bind_local {
                    self.do_bind(local)?;
                } else {
                    self.bind_ephemeral(remote.addr)?;
                }
            }
            let guard = self.inner.read();
            return match guard.as_ref() {
                Some(RawInner::Bound(inner)) => {
                    inner.connect(remote.addr);
                    Ok(())
                }
                Some(RawInner::Wildcard(inner)) => {
                    inner.connect(remote.addr);
                    Ok(())
                }
                _ => Err(SystemError::EINVAL),
            };
        }
        Err(SystemError::EAFNOSUPPORT)
    }

    fn validate_sendto_addr(
        &self,
        addr: *const crate::net::posix::SockAddr,
        addrlen: u32,
    ) -> Result<(), SystemError> {
        RawSocket::validate_sendto_addr(self, addr, addrlen)
    }

    fn shutdown(&self, _how: crate::net::socket::common::ShutdownBit) -> Result<(), SystemError> {
        // Raw socket 的 shutdown 在 connect 前返回 ENOTCONN；connect 后为 no-op。
        let connected = match self.inner.read().as_ref() {
            Some(RawInner::Bound(b) | RawInner::Wildcard(b)) => b.remote_addr().is_some(),
            _ => false,
        };
        if connected {
            Ok(())
        } else {
            Err(SystemError::ENOTCONN)
        }
    }

    fn listen(&self, _backlog: usize) -> Result<(), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn accept(&self) -> Result<(Arc<dyn crate::net::socket::Socket>, Endpoint), SystemError> {
        Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
    }

    fn send(&self, buffer: &[u8], flags: PMSG) -> Result<usize, SystemError> {
        RawSocket::send(self, buffer, flags)
    }

    fn send_to(&self, buffer: &[u8], flags: PMSG, address: Endpoint) -> Result<usize, SystemError> {
        RawSocket::send_to(self, buffer, flags, address)
    }

    fn recv(&self, buffer: &mut [u8], flags: PMSG) -> Result<usize, SystemError> {
        RawSocket::recv(self, buffer, flags)
    }

    fn recv_from(
        &self,
        buffer: &mut [u8],
        flags: PMSG,
        address: Option<Endpoint>,
    ) -> Result<(usize, Endpoint), SystemError> {
        RawSocket::recv_from(self, buffer, flags, address)
    }

    fn do_close(&self) -> Result<(), SystemError> {
        self.close();
        Ok(())
    }

    fn remote_endpoint(&self) -> Result<Endpoint, SystemError> {
        // Linux 语义：raw socket 的 getpeername(2) 即使 connect 之后也返回 ENOTCONN。
        Err(SystemError::ENOTCONN)
    }

    fn local_endpoint(&self) -> Result<Endpoint, SystemError> {
        let proto = self.protocol_u16();
        match self.inner.read().as_ref() {
            Some(RawInner::Bound(bound)) => {
                if let Some(addr) = bound.local_addr() {
                    Ok(Endpoint::Ip(smoltcp::wire::IpEndpoint::new(addr, proto)))
                } else {
                    Ok(self.unspecified_endpoint(proto))
                }
            }
            _ => Ok(self.unspecified_endpoint(proto)),
        }
    }

    fn recv_msg(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
        flags: PMSG,
    ) -> Result<usize, SystemError> {
        RawSocket::recv_msg(self, msg, flags)
    }

    fn send_msg(&self, msg: &crate::net::posix::MsgHdr, flags: PMSG) -> Result<usize, SystemError> {
        RawSocket::send_msg(self, msg, flags)
    }

    fn epoll_items(&self) -> &crate::net::socket::common::EPollItems {
        &self.epoll_items
    }

    fn fasync_items(&self) -> &crate::filesystem::vfs::fasync::FAsyncItems {
        &self.fasync_items
    }

    fn check_io_event(&self) -> EPollEventType {
        let mut event = EPollEventType::empty();

        if !self.loopback_rx.lock_irqsave().pkts.is_empty() {
            event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
        }

        match self.inner.read().as_ref() {
            None | Some(RawInner::Unbound(_)) => {
                event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
            }
            Some(RawInner::Wildcard(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
            Some(RawInner::Bound(bound)) => {
                let (can_recv, can_send) =
                    bound.with_socket(|socket| (socket.can_recv(), socket.can_send()));

                if can_recv {
                    event.insert(EP::EPOLLIN | EP::EPOLLRDNORM);
                }

                if can_send {
                    event.insert(EP::EPOLLOUT | EP::EPOLLWRNORM | EP::EPOLLWRBAND);
                }
            }
        }
        event
    }

    fn socket_inode_id(&self) -> crate::filesystem::vfs::InodeId {
        self.inode_id
    }

    fn option(&self, level: PSOL, name: usize, value: &mut [u8]) -> Result<usize, SystemError> {
        match level {
            PSOL::SOCKET => self.option_socket_level(name, value),
            PSOL::RAW => self.option_raw_level(name, value),
            PSOL::IP => self.option_ip_level(name, value),
            PSOL::IPV6 => self.option_ipv6_level(name, value),
            PSOL::ICMPV6 => self.option_icmpv6_level(name, value),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn set_option(&self, level: PSOL, name: usize, val: &[u8]) -> Result<(), SystemError> {
        match level {
            PSOL::SOCKET => self.set_option_socket_level(name, val),
            PSOL::RAW => self.set_option_raw_level(name, val),
            PSOL::IP => self.set_option_ip_level(name, val),
            PSOL::IPV6 => self.set_option_ipv6_level(name, val),
            PSOL::ICMPV6 => self.set_option_icmpv6_level(name, val),
            _ => Err(SystemError::ENOPROTOOPT),
        }
    }

    fn recv_bytes_available(&self) -> Result<usize, SystemError> {
        let guard = self.inner.read();
        Ok(match *guard {
            Some(RawInner::Wildcard(ref bound)) => {
                bound.with_mut_socket(|socket| match socket.peek() {
                    Ok(payload) => payload.len(),
                    Err(_) => 0,
                })
            }
            Some(RawInner::Bound(ref bound)) => {
                bound.with_mut_socket(|socket| match socket.peek() {
                    Ok(payload) => payload.len(),
                    Err(_) => 0,
                })
            }
            _ => 0,
        })
    }

    fn send_bytes_available(&self) -> Result<usize, SystemError> {
        let guard = self.inner.read();
        Ok(match *guard {
            Some(RawInner::Wildcard(ref bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity() - socket.send_queue())
            }
            Some(RawInner::Bound(ref bound)) => {
                bound.with_socket(|socket| socket.payload_send_capacity() - socket.send_queue())
            }
            _ => 0,
        })
    }
}

impl InetSocket for RawSocket {
    fn on_iface_events(&self) {
        // Raw socket 不需要特殊的接口事件处理
    }
}
