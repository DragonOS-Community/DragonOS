//! Raw protocol PMTU feedback. Called after ingress/interface locks drop.
use super::{inner::RawInner, RawSocket};
use crate::{
    net::{
        pmtu::PmtuFeedback,
        socket::inet::{
            common::{
                error_queue::{
                    ErrorPacketMetadata, ErrorQueueEntry, SockExtendedErr, SO_EE_ORIGIN_ICMP,
                    SO_EE_ORIGIN_ICMP6, SO_EE_ORIGIN_LOCAL,
                },
                pmtu::PmtuPolicy,
            },
            InetSocket,
        },
    },
    process::namespace::net_namespace::NetNamespace,
};
use alloc::{sync::Arc, vec::Vec};
use smoltcp::wire::{IpAddress, IpEndpoint, IpProtocol, IpVersion};
use system_error::SystemError;

/// Like a socket dst hint, this remembers the last actual output addresses,
/// never a device reference or an obsolete route/MTU value. Queries re-route
/// the actual destination through the current namespace FIB.
#[derive(Debug, Clone, Copy)]
pub(super) struct RawOutputPath {
    original_destination: IpAddress,
    bound_source: Option<IpAddress>,
    required_oif: Option<u32>,
    actual: crate::net::output::SocketRouteHint,
}

pub(crate) fn handle_pmtu_feedback(netns: &Arc<NetNamespace>, feedback: &PmtuFeedback) {
    if feedback.protocol == IpProtocol::Unknown(255) {
        return;
    }
    let version = match feedback.source {
        IpAddress::Ipv4(_) => IpVersion::Ipv4,
        IpAddress::Ipv6(_) => IpVersion::Ipv6,
    };
    for listener in super::loopback::snapshot_raw_ingress_listeners(netns) {
        // The quoted packet travels from local to remote, unlike normal RX.
        if listener.matches(
            version,
            feedback.protocol,
            feedback.destination,
            feedback.source,
            feedback.ingress_ifindex,
        ) {
            listener.socket().receive_pmtu(feedback);
        }
    }
}

impl RawSocket {
    pub(super) fn remember_output_path(
        &self,
        destination: IpAddress,
        bound_source: Option<IpAddress>,
        required_oif: Option<u32>,
        actual: Option<crate::net::output::SocketRouteHint>,
    ) {
        if let Some(actual) = actual {
            *self.output_path.lock() = Some(RawOutputPath {
                original_destination: destination,
                bound_source: bound_source.filter(|address| !address.is_unspecified()),
                required_oif,
                actual,
            });
        }
    }

    pub(super) fn remember_hdrincl_output_path(
        &self,
        destination: IpAddress,
        header_destination: IpAddress,
        bound_source: Option<IpAddress>,
        required_oif: Option<u32>,
        actual: Option<crate::net::output::SocketRouteHint>,
    ) {
        // With HDRINCL Linux permits sockaddr and the supplied header dst to
        // differ. Without DNAT the original sockaddr remains the route key;
        // a packet-address-only hint cannot describe that path. Fall back to
        // the ordinary connected FIB lookup rather than caching the wrong dst.
        if destination != header_destination
            && actual.is_some_and(|hint| hint.destination == header_destination)
        {
            self.output_path.lock().take();
        } else {
            self.remember_output_path(destination, bound_source, required_oif, actual);
        }
    }
    pub(super) fn take_pending_error(&self) -> Result<(), SystemError> {
        let errno = {
            let mut errors = self.errors.lock();
            let errno = errors.0;
            errors.0 = 0;
            errno
        };
        match SystemError::from_posix_errno(-errno) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn receive_pmtu(&self, feedback: &PmtuFeedback) {
        let connected = match self.inner.read().as_ref() {
            Some(RawInner::Bound(bound) | RawInner::Wildcard(bound)) => {
                if bound
                    .local_addr()
                    .is_some_and(|address| !address.is_unspecified() && address != feedback.source)
                    || bound
                        .remote_addr()
                        .is_some_and(|address| address != feedback.destination)
                {
                    return;
                }
                bound.remote_addr().is_some()
            }
            Some(RawInner::Unbound(_)) => false,
            None => return,
        };
        let ifindex = self.device_binding.ifindex();
        if ifindex != 0 && ifindex != feedback.ingress_ifindex as usize {
            return;
        }
        let options = self.options.read().clone();
        let ipv6 = self.ip_version == IpVersion::Ipv6;
        let policy = if ipv6 {
            options.ipv6_pmtu
        } else {
            options.ip_pmtu
        };
        let recverr = if ipv6 {
            options.recv_err_v6
        } else {
            options.recv_err_v4
        };
        // Linux rawv6_err applies connection/RECVERR before learning; raw_err
        // learns first. rawv6 does not apply UDP's accept_pmtu predicate.
        if ipv6 && !connected && !recverr {
            return;
        }
        if ipv6 || policy.accepts_updates() {
            let required_oif = match self.device_binding.ifindex() {
                0 => None,
                index => Some(index as u32),
            };
            let _ = crate::net::route::pmtu::learn(&self.netns, feedback, required_oif);
        }
        if !connected && !recverr {
            return;
        }
        let harderr = if ipv6 {
            policy == PmtuPolicy::Do
        } else {
            policy != PmtuPolicy::Dont
        };
        if !harderr && !recverr {
            return;
        }
        let mut entry = None;
        if recverr {
            let offset = if options.ip_hdrincl {
                0
            } else {
                feedback.transport_offset
            };
            let data = feedback.quote.get(offset..).unwrap_or(&[]);
            let mut payload = Vec::new();
            if payload.try_reserve_exact(data.len()).is_ok() {
                payload.extend_from_slice(data);
                entry = Some(ErrorQueueEntry {
                    error: SockExtendedErr {
                        ee_errno: (-SystemError::EMSGSIZE.to_posix_errno()) as u32,
                        ee_origin: if ipv6 {
                            SO_EE_ORIGIN_ICMP6
                        } else {
                            SO_EE_ORIGIN_ICMP
                        },
                        ee_type: if ipv6 { 2 } else { 3 },
                        ee_code: if ipv6 { 0 } else { 4 },
                        ee_info: feedback.mtu,
                        ..Default::default()
                    },
                    offender: Some(feedback.offender),
                    destination: IpEndpoint::new(feedback.destination, 0),
                    payload,
                    ipv6,
                    ingress_ifindex: feedback.ingress_ifindex,
                    packet: Some(ErrorPacketMetadata {
                        ttl: feedback.outer_ttl,
                        tos: feedback.outer_tos,
                        local_address: feedback.source,
                    }),
                });
            }
        }
        {
            let mut errors = self.errors.lock();
            if let Some(entry) = entry {
                errors.1.push(entry, options.sock_rcvbuf as usize);
            }
            errors.0 = -SystemError::EMSGSIZE.to_posix_errno();
        }
        self.notify();
    }

    pub(super) fn local_mtu_error(&self, destination: IpAddress, mtu: usize) {
        let options = self.options.read().clone();
        let ipv6 = matches!(destination, IpAddress::Ipv6(_));
        if !(if ipv6 {
            options.recv_err_v6
        } else {
            options.recv_err_v4
        }) {
            return;
        }
        let entry = ErrorQueueEntry {
            error: SockExtendedErr {
                ee_errno: (-SystemError::EMSGSIZE.to_posix_errno()) as u32,
                ee_origin: SO_EE_ORIGIN_LOCAL,
                ee_info: mtu as u32,
                ..Default::default()
            },
            offender: None,
            destination: IpEndpoint::new(
                destination,
                if ipv6 {
                    0
                } else {
                    self.connected_port
                        .load(core::sync::atomic::Ordering::Acquire) as u16
                },
            ),
            payload: Vec::new(),
            ipv6,
            ingress_ifindex: 0,
            packet: None,
        };
        let queued = self
            .errors
            .lock()
            .1
            .push(entry, options.sock_rcvbuf as usize);
        if queued {
            self.notify();
        }
    }

    pub(super) fn connected_path_mtu(&self) -> Result<usize, SystemError> {
        let (destination, source) = match self.inner.read().as_ref() {
            Some(RawInner::Bound(bound) | RawInner::Wildcard(bound)) => (
                bound.remote_addr().ok_or(SystemError::ENOTCONN)?,
                bound
                    .local_addr()
                    .filter(|address| !address.is_unspecified()),
            ),
            _ => return Err(SystemError::ENOTCONN),
        };
        let required_oif = self
            .device_binding
            .resolve_iface(&self.netns)?
            .map(|iface| iface.nic_id() as u32);
        let required_oif =
            if destination.is_multicast() && matches!(destination, IpAddress::Ipv4(_)) {
                required_oif.or_else(|| {
                    u32::try_from(
                        self.ip_multicast_ifindex
                            .load(core::sync::atomic::Ordering::Acquire),
                    )
                    .ok()
                    .filter(|index| *index != 0)
                })
            } else {
                required_oif
            };
        let hint = *self.output_path.lock();
        if let Some(hint) = hint.filter(|hint| {
            hint.original_destination == destination
                && hint.bound_source == source
                && hint.required_oif == required_oif
                && hint.actual.rules_generation == self.netns.nftables().generation()
        }) {
            let router = self.netns.router();
            let route = {
                let routes =
                    crate::net::route::lock_output_routes(&router, self.netns.device_list());
                routes
                    .lookup(hint.actual.destination, required_oif)
                    .ok_or(SystemError::ENETUNREACH)?
            };
            return Ok(crate::net::route::pmtu::path_mtu(
                &self.netns,
                route,
                hint.actual.source,
                hint.actual.destination,
            )
            .path);
        }
        let (route, source) = match destination {
            IpAddress::Ipv4(_) => {
                let resolved = crate::net::route::resolve_ipv4_route(
                    &self.netns,
                    destination,
                    required_oif,
                    source,
                )?;
                (resolved.output_decision(), resolved.source)
            }
            IpAddress::Ipv6(_) => {
                let resolved = crate::net::route::resolve_ipv6_send_route(
                    &self.netns,
                    destination,
                    required_oif,
                    source,
                )?;
                (resolved.decision, IpAddress::Ipv6(resolved.source))
            }
        };
        Ok(crate::net::route::pmtu::path_mtu(&self.netns, route, source, destination).path)
    }
}
