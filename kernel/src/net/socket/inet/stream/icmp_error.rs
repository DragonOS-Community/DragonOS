//! TCP control errors: association/state stay under the transport lock; the
//! socket owns only Linux's bounded extended-error ABI queue.
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use smoltcp::{
    socket::tcp,
    wire::{IpEndpoint, IpVersion, TcpSeqNumber},
};
use system_error::SystemError;

use super::{inner, TcpSocket};
use crate::net::{
    icmp_error::IcmpErrorFeedback,
    socket::{
        inet::common::error_queue::{
            ErrorCmsgOptions, ErrorPacketMetadata, ErrorQueueEntry, SockExtendedErr,
            SO_EE_ORIGIN_ICMP, SO_EE_ORIGIN_ICMP6,
        },
        PMSG,
    },
};

pub(super) fn network_reason(error: SystemError) -> tcp::NetworkError {
    use tcp::NetworkError as N;
    match error {
        SystemError::ENETUNREACH => N::NetworkUnreachable,
        SystemError::EHOSTUNREACH => N::HostUnreachable,
        SystemError::ENOPROTOOPT => N::ProtocolUnreachable,
        SystemError::ECONNREFUSED => N::ConnectionRefused,
        SystemError::EACCES => N::PermissionDenied,
        SystemError::EHOSTDOWN => N::HostDown,
        SystemError::ENONET => N::NoNetwork,
        SystemError::EOPNOTSUPP_OR_ENOTSUP => N::OperationNotSupported,
        SystemError::EMSGSIZE => N::MessageTooLarge,
        _ => N::ProtocolError,
    }
}

fn associated(socket: &tcp::Socket, feedback: &IcmpErrorFeedback) -> bool {
    feedback.seq.is_some_and(|seq| {
        socket.accepts_error_quote(
            IpEndpoint::new(feedback.source, feedback.src_port),
            IpEndpoint::new(feedback.destination, feedback.dst_port),
            TcpSeqNumber(seq),
            feedback.ingress_ifindex,
        )
    })
}

fn is_handshake(state: tcp::State) -> bool {
    matches!(state, tcp::State::SynSent | tcp::State::SynReceived)
}

fn abort_passive_request(feedback: &IcmpErrorFeedback) -> bool {
    if feedback.source.version() == IpVersion::Ipv6 {
        feedback.hard_error()
    } else {
        // Linux tcp_v4_err passes this request-specific policy, not the UDP
        // error table's fatal bit. Port/protocol-unreachable do not drop a SYNACK.
        matches!(feedback.icmp_type, 11 | 12)
            || (feedback.icmp_type == 3 && matches!(feedback.icmp_code, 0 | 1))
    }
}

impl TcpSocket {
    pub(super) fn take_socket_option_error(&self) -> Option<SystemError> {
        let guard = self.inner.read();
        let state = guard.as_ref()?;
        let hard = match state {
            inner::Inner::Connecting(owner) => owner.take_error(),
            inner::Inner::Established(owner) => {
                owner.with_mut(|socket| socket.take_error().map(inner::transport_error))
            }
            _ => None,
        };
        hard.or_else(|| {
            let mut take = |socket: &mut tcp::Socket| {
                socket.take_soft_error().map(|reason| {
                    inner::transport_error(tcp::PendingError {
                        reason: tcp::ErrorReason::Network(reason),
                        state: socket.state(),
                    })
                })
            };
            match state {
                inner::Inner::Connecting(owner) => owner.with_mut(&mut take),
                inner::Inner::Established(owner) => owner.with_mut(&mut take),
                _ => None,
            }
        })
    }

    pub(super) fn set_recv_error_policy(&self, version: IpVersion, enabled: bool) {
        let index = usize::from(version == IpVersion::Ipv6);
        let mut guard = self.inner.write();
        self.options.recv_errors[index].store(enabled, Ordering::Release);
        if let Some(state) = guard.as_mut() {
            match state {
                inner::Inner::Listening(listener) => {
                    listener.set_recv_error_policy(version, enabled)
                }
                state => {
                    state.with_data_socket_mut(|socket| {
                        socket.set_recv_error_policy(version, enabled)
                    });
                }
            }
        }
        if !enabled {
            // Same inner -> queue order as ingress and ERRQUEUE consume.
            self.error_queue.lock().clear();
        }
    }

    pub(super) fn recv_error_policy(&self, version: IpVersion) -> bool {
        self.options.recv_errors[usize::from(version == IpVersion::Ipv6)].load(Ordering::Acquire)
    }

    pub(super) fn error_queue_ready(&self) -> bool {
        !self.error_queue.lock().is_empty()
    }

    pub(super) fn recv_icmp_error(
        &self,
        msg: &mut crate::net::posix::MsgHdr,
    ) -> Result<usize, SystemError> {
        let entry = {
            let guard = self.inner.read();
            let consume = |previous: Option<SystemError>| {
                let mut queue = self.error_queue.lock();
                let entry = queue.pop()?;
                let next = queue
                    .pending_after_pop(&entry, previous.map_or(0, |error| -error.to_posix_errno()));
                Some((SystemError::from_posix_errno(-next), entry))
            };
            match guard.as_ref() {
                Some(inner::Inner::Connecting(owner)) => owner.consume_error_queue(consume),
                Some(inner::Inner::Established(owner)) => owner.with_mut(|socket| {
                    let (next, entry) =
                        consume(socket.pending_error().map(inner::transport_error))?;
                    socket.take_error();
                    if let Some(error) = next {
                        socket.set_reported_network_error(network_reason(error));
                    }
                    Some(entry)
                }),
                _ => self.error_queue.lock().pop(),
            }
        }
        .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        self.update_events();
        let result = entry.recv_msg_with_options(msg, ErrorCmsgOptions::default());
        msg.msg_flags |= PMSG::ERRQUEUE.bits() as i32;
        result
    }

    pub(crate) fn receive_icmp_error(&self, feedback: &IcmpErrorFeedback) {
        debug_assert!(feedback.mtu().is_none());
        let snapshot = {
            let guard = self.inner.read();
            let Some(state) = guard.as_ref() else {
                return;
            };
            if let inner::Inner::Listening(listener) = state {
                for bound in &listener.inners {
                    bound.with_mut::<tcp::Socket, _, _>(|socket| {
                        if associated(socket, feedback) {
                            let report = socket.recv_error_policies()
                                [usize::from(feedback.source.version() == IpVersion::Ipv6)];
                            socket.report_network_error(
                                network_reason(feedback.errno()),
                                report,
                                abort_passive_request(feedback),
                            );
                        }
                    });
                }
                return;
            }
            if !matches!(
                state,
                inner::Inner::Connecting(_) | inner::Inner::Established(_)
            ) {
                return;
            }
            state
                .with_socket(|socket| associated(socket, feedback).then_some(socket.state()))
                .flatten()
        };
        let Some(before) = snapshot else {
            return;
        };
        // Linux IPv4 queues SYN errors even without IP_RECVERR. IPv6's helper
        // is explicitly gated by IPV6_RECVERR. Established errors only sk_err.
        // Allocate before the commit guards even if RECVERR is currently off:
        // a racing setter may enable it before association is revalidated.
        let queue_syn = is_handshake(before);
        let mut entry = if queue_syn {
            self.make_icmp_entry(feedback)
        } else {
            None
        };
        let guard = self.inner.read();
        let Some(state) = guard.as_ref() else {
            return;
        };
        if !matches!(
            state,
            inner::Inner::Connecting(_) | inner::Inner::Established(_)
        ) {
            return;
        }
        // This guard prevents connect/close changing protocol identity while
        // the transport lock revalidates the quoted generation/sequence.
        let mut apply = |socket: &mut tcp::Socket| {
            if !associated(socket, feedback) {
                return;
            }
            let report = socket.recv_error_policies()
                [usize::from(feedback.source.version() == IpVersion::Ipv6)];
            if is_handshake(socket.state())
                && (feedback.source.version() == IpVersion::Ipv4 || report)
            {
                if let Some(entry) = entry.take() {
                    self.error_queue
                        .lock()
                        .push(entry, self.options.recv_buf_size.load(Ordering::Relaxed));
                }
            }
            socket.report_network_error(network_reason(feedback.errno()), report, false);
        };
        match state {
            inner::Inner::Connecting(owner) => owner.with_mut(&mut apply),
            inner::Inner::Established(owner) => owner.with_mut(&mut apply),
            _ => {}
        }
    }

    fn make_icmp_entry(&self, feedback: &IcmpErrorFeedback) -> Option<ErrorQueueEntry> {
        // Linux TCP passes the quoted TCP header (not application payload).
        let bytes = feedback.quote.get(feedback.transport_offset..)?;
        let mut payload = Vec::new();
        payload.try_reserve_exact(bytes.len()).ok()?;
        payload.extend_from_slice(bytes);
        let ipv6_packet = feedback.source.version() == IpVersion::Ipv6;
        let abi = |address| match (self.ip_version, address) {
            (IpVersion::Ipv6, smoltcp::wire::IpAddress::Ipv4(address)) => {
                smoltcp::wire::IpAddress::Ipv6(address.to_ipv6_mapped())
            }
            _ => address,
        };
        Some(ErrorQueueEntry {
            error: SockExtendedErr {
                ee_errno: feedback.errno().to_posix_errno().unsigned_abs(),
                ee_origin: if ipv6_packet {
                    SO_EE_ORIGIN_ICMP6
                } else {
                    SO_EE_ORIGIN_ICMP
                },
                ee_type: feedback.icmp_type,
                ee_code: feedback.icmp_code,
                ee_info: feedback.info(),
                ..SockExtendedErr::default()
            },
            offender: Some(abi(feedback.offender)),
            destination: IpEndpoint::new(abi(feedback.destination), feedback.dst_port),
            payload,
            ipv6: self.ip_version == IpVersion::Ipv6,
            ingress_ifindex: feedback.ingress_ifindex,
            packet: Some(ErrorPacketMetadata {
                ttl: feedback.outer_ttl,
                tos: feedback.outer_tos,
                local_address: feedback.source,
            }),
        })
    }
}
