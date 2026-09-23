//! Namespace TCP output uses the same admission and deferred-neighbor queues as
//! interface-local output, without inventing an interface to own TCP state.

use super::*;

#[derive(Debug)]
pub(crate) struct TcpOutputQueue(LocalInputQueue);

impl TcpOutputQueue {
    pub(crate) fn new() -> Self {
        Self(LocalInputQueue::new())
    }

    pub(crate) fn device<'a>(
        &'a self,
        routes: &'a crate::net::route::OutputRouteGuard<'a>,
    ) -> TcpOutputDevice<'a> {
        TcpOutputDevice {
            queue: &self.0,
            policy: OutputBackendPolicy {
                routes,
                configured_neighbors: None,
                // A transport context is not a netdev. Every transmission is
                // classified against the namespace FIB, including local IPs.
                owner_ifindex: 0,
                owner_is_up: false,
                authoritative_output: true,
            },
        }
    }

    /// Transfer admitted packets to the selected real device. Neighbor waits
    /// and TX-completion retries thereafter belong exclusively to that device.
    pub(crate) fn drain(&self, netns: &Arc<NetNamespace>, budget: usize) -> bool {
        let Some(guard) = self.0.try_begin_output_drain() else {
            return self.0.has_output();
        };
        for _ in 0..budget {
            let (packet, reservation) = match self
                .0
                .pop_ready_output(crate::time::Instant::now().into(), false)
            {
                LocalOutputPop::Ready(packet, reservation, _) => (packet, reservation),
                LocalOutputPop::Empty => return self.0.finish_output_drain(guard),
                // This queue never owns neighbor or physical TX retry state.
                LocalOutputPop::DeferredUntil(_) => unreachable!(),
            };
            match packet.disposition {
                LocalOutputDisposition::Routed { oif, .. } => {
                    let iface = netns.device_list().get(&(oif as usize)).cloned();
                    let Some(iface) = iface else {
                        drop(reservation);
                        self.0.recycle_output(packet.frame);
                        continue;
                    };
                    match iface.common().enqueue_existing_deferred_output(packet) {
                        ExistingDeferredEnqueue::Queued(retry_at) => {
                            iface.common().schedule_registered_local_output(retry_at);
                        }
                        ExistingDeferredEnqueue::Missing(packet, admission) => {
                            match transmit_admitted_routed_output(iface.as_ref(), packet, admission)
                            {
                                AdmittedRoutedOutput::Sent(packet)
                                | AdmittedRoutedOutput::Drop(packet, _) => {
                                    self.0.recycle_output(packet.frame);
                                }
                                AdmittedRoutedOutput::Queued(retry_at) => {
                                    iface.common().schedule_registered_local_output(retry_at);
                                }
                            }
                        }
                        ExistingDeferredEnqueue::Full(packet) => {
                            // Device admission is bounded. Congestion may drop
                            // a packet, just as on a physical transmit queue.
                            self.0.recycle_output(packet.frame);
                        }
                    };
                }
                LocalOutputDisposition::Local { oif, ip_mtu } => {
                    let iface = netns.device_list().get(&(oif as usize)).cloned();
                    if let Some(iface) = iface {
                        if packet.frame.len() <= ip_mtu && packet.frame.len() <= iface.mtu() {
                            let _ = iface.inject_local_ip_packet(
                                oif,
                                iface.mac(),
                                &packet.frame,
                                false,
                            );
                        }
                    }
                    self.0.recycle_output(packet.frame);
                }
                LocalOutputDisposition::Drop => self.0.recycle_output(packet.frame),
                LocalOutputDisposition::NativeOwner => {
                    unreachable!("namespace TCP has no native device")
                }
            }
            drop(reservation);
        }
        self.0.finish_output_drain(guard)
    }
}

pub(crate) struct TcpOutputDevice<'a> {
    queue: &'a LocalInputQueue,
    policy: OutputBackendPolicy<'a>,
}

pub(crate) struct TcpTxToken<'a>(LocalInputTxToken<'a>);

impl SmolTxToken for TcpTxToken<'_> {
    fn consume<R, F: FnOnce(&mut [u8]) -> R>(self, len: usize, f: F) -> R {
        self.0.consume(len, f)
    }

    fn set_meta(&mut self, meta: PacketMeta) {
        self.0.set_meta(meta);
    }

    fn egress_override(
        &mut self,
        version: smoltcp::wire::IpVersion,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
    ) -> Result<Option<smoltcp::phy::TxEgressOverride>, smoltcp::phy::TxEgressError> {
        self.0.egress_override(version, destination, meta)
    }

    fn apply_egress_override(
        &mut self,
        egress: Option<smoltcp::phy::TxEgressOverride>,
    ) -> Result<(), smoltcp::phy::TxEgressError> {
        self.0.apply_egress_override(egress)
    }
}

/// TCP ingress is passed explicitly after IP validation, not through Device RX.
pub(crate) struct NoTcpRx;

impl RxToken for NoTcpRx {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, _f: F) -> R {
        unreachable!()
    }
}

impl SmolDevice for TcpOutputDevice<'_> {
    type RxToken<'a>
        = NoTcpRx
    where
        Self: 'a;
    type TxToken<'a>
        = TcpTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        None
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        local_tx_token(self.queue, self.policy, self.capabilities()).map(TcpTxToken)
    }

    fn capabilities(&self) -> DeviceCapabilities {
        transport_capabilities()
    }

    fn outbound_ip_mtu(&self, destination: smoltcp::wire::IpAddress, meta: PacketMeta) -> usize {
        self.policy
            .outbound_ip_mtu(destination, meta, u16::MAX as usize)
    }
}

pub(crate) fn transport_capabilities() -> DeviceCapabilities {
    let mut caps = DeviceCapabilities::default();
    caps.medium = smoltcp::phy::Medium::Ip;
    caps.max_transmission_unit = u16::MAX as usize;
    caps
}

pub(crate) fn new_transport_interface() -> smoltcp::iface::Interface {
    // Interface construction only needs capabilities and a random seed. This
    // object is a transport context, never a registered or configurable netdev.
    struct Initializer;
    impl SmolDevice for Initializer {
        type RxToken<'a> = NoTcpRx;
        type TxToken<'a> = TcpTxToken<'a>;
        fn receive(&mut self, _: smoltcp::time::Instant) -> Option<(NoTcpRx, TcpTxToken<'_>)> {
            None
        }
        fn transmit(&mut self, _: smoltcp::time::Instant) -> Option<TcpTxToken<'_>> {
            None
        }
        fn capabilities(&self) -> DeviceCapabilities {
            transport_capabilities()
        }
    }
    let mut config = smoltcp::iface::Config::new(smoltcp::wire::HardwareAddress::Ip);
    config.random_seed = crate::arch::rand::rand() as u64;
    // The root namespace is constructed by sysfs before timekeeping starts.
    // This empty transport context has no timers; connect/poll supplies the
    // current monotonic time before any protocol state becomes active.
    smoltcp::iface::Interface::new(config, &mut Initializer, smoltcp::time::Instant::ZERO)
}
