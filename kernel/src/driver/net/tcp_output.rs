//! Namespace TCP output uses the same admission and deferred-neighbor queues as
//! interface-local output, without inventing an interface to own TCP state.

use super::*;

#[derive(Debug)]
pub(crate) struct TcpOutputQueue {
    queue: LocalInputQueue,
    retry_backoff_us: AtomicU64,
}

pub(crate) struct TcpOutputDrainResult {
    pub(crate) immediate: bool,
    pub(crate) retry_at: Option<smoltcp::time::Instant>,
}

impl TcpOutputQueue {
    const RETRY_BACKOFF_MIN_US: u64 = 1_000;
    const RETRY_BACKOFF_MAX_US: u64 = 1_000_000;

    pub(crate) fn new() -> Self {
        Self {
            queue: LocalInputQueue::new(),
            retry_backoff_us: AtomicU64::new(Self::RETRY_BACKOFF_MIN_US),
        }
    }

    pub(crate) fn device<'a>(
        &'a self,
        netns: &'a NetNamespace,
        routes: &'a crate::net::route::OutputRouteGuard<'a>,
        ruleset: &'a crate::net::nftables::RulesetSnapshot,
        device_names: &'a crate::net::nftables::NftDeviceNames,
    ) -> TcpOutputDevice<'a> {
        TcpOutputDevice {
            queue: &self.queue,
            policy: OutputBackendPolicy {
                netns,
                routes,
                ruleset: Some(ruleset),
                device_names,
                configured_neighbors: None,
                // A transport context is not a netdev. Every transmission is
                // classified against the namespace FIB, including local IPs.
                owner_ifindex: 0,
                owner_is_up: false,
                authoritative_output: true,
            },
        }
    }

    /// Transfer routed packets to their real egress queue and deliver local
    /// packets through the shared fragmentation path. Neighbor waits and
    /// physical TX retries belong exclusively to the selected device.
    fn poll_state(&self) -> TcpOutputDrainResult {
        let now: smoltcp::time::Instant = crate::time::Instant::now().into();
        let output = self.queue.output.lock();
        let retry_at = output.backpressured.front().map(|queued| queued.retry_at);
        TcpOutputDrainResult {
            immediate: !output.packets.is_empty() || retry_at.is_some_and(|at| at <= now),
            retry_at,
        }
    }

    pub(crate) fn drain(&self, netns: &Arc<NetNamespace>, budget: usize) -> TcpOutputDrainResult {
        let Some(guard) = self.queue.try_begin_output_drain() else {
            return self.poll_state();
        };
        for _ in 0..budget {
            let (packet, reservation) = match self
                .queue
                .pop_ready_output(crate::time::Instant::now().into(), false)
            {
                LocalOutputPop::Ready(packet, reservation, _) => (packet, reservation),
                LocalOutputPop::Empty => {
                    self.queue.finish_output_drain(guard);
                    return self.poll_state();
                }
                // Local fragment allocation may be deferred. The namespace
                // deadline poller will revisit the queue when it is due.
                LocalOutputPop::DeferredUntil(_) => {
                    drop(guard);
                    return self.poll_state();
                }
            };
            match packet.disposition {
                LocalOutputDisposition::Routed { oif, .. } => {
                    let iface = netns.device_list().get(&(oif as usize)).cloned();
                    let Some(iface) = iface else {
                        drop(reservation);
                        self.queue.recycle_output(packet.frame);
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
                                    self.queue.recycle_output(packet.frame);
                                }
                                AdmittedRoutedOutput::Queued(retry_at) => {
                                    iface.common().schedule_registered_local_output(retry_at);
                                }
                            }
                        }
                        ExistingDeferredEnqueue::Full(packet) => {
                            // Device admission is bounded. Congestion may drop
                            // a packet, just as on a physical transmit queue.
                            self.queue.recycle_output(packet.frame);
                        }
                    };
                }
                LocalOutputDisposition::Local { .. } => {
                    // The shared local-output path owns IPv4 fragmentation and
                    // advances the cursor only after each fragment is accepted.
                    let result =
                        transmit_local_stack_output(netns, false, &mut TransportOnlyDevice, packet);
                    match result {
                        LocalOutputTransmitResult::Continue(packet) => {
                            self.retry_backoff_us
                                .store(Self::RETRY_BACKOFF_MIN_US, Ordering::Release);
                            reservation.requeue_ready(packet);
                            continue;
                        }
                        LocalOutputTransmitResult::RetrySoon(packet) => {
                            let delay_us = self
                                .retry_backoff_us
                                .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                                    Some(current.saturating_mul(2).min(Self::RETRY_BACKOFF_MAX_US))
                                })
                                .unwrap_or(Self::RETRY_BACKOFF_MAX_US);
                            let retry_at: smoltcp::time::Instant =
                                crate::time::Instant::now().into();
                            let retry_at =
                                retry_at + smoltcp::time::Duration::from_micros(delay_us);
                            reservation.requeue_backpressured(packet, retry_at);
                            continue;
                        }
                        LocalOutputTransmitResult::Sent(packet) => {
                            self.retry_backoff_us
                                .store(Self::RETRY_BACKOFF_MIN_US, Ordering::Release);
                            self.queue.recycle_output(packet.frame);
                        }
                        LocalOutputTransmitResult::Drop(packet, error) => {
                            log::debug!("dropping namespace TCP local output: {:?}", error);
                            self.retry_backoff_us
                                .store(Self::RETRY_BACKOFF_MIN_US, Ordering::Release);
                            self.queue.recycle_output(packet.frame);
                        }
                        LocalOutputTransmitResult::RetryAt { .. } => {
                            unreachable!("local delivery does not resolve neighbors")
                        }
                    }
                }
                LocalOutputDisposition::Drop => self.queue.recycle_output(packet.frame),
                LocalOutputDisposition::NativeOwner => {
                    unreachable!("namespace TCP has no native device")
                }
            }
            drop(reservation);
        }
        self.queue.finish_output_drain(guard);
        self.poll_state()
    }
}

pub(crate) struct TcpOutputDevice<'a> {
    queue: &'a LocalInputQueue,
    policy: OutputBackendPolicy<'a>,
}

pub(crate) struct TcpTxToken<'a>(LocalInputTxToken<'a>);

impl SmolTxToken for TcpTxToken<'_> {
    fn deferred_ip_output(&self, _: smoltcp::wire::IpVersion) -> bool {
        true
    }

    fn consume_full_ip<F>(
        self,
        len: usize,
        meta: PacketMeta,
        class: smoltcp::phy::IpOutputClass,
        ipv4_fragment_ident: Option<u16>,
        emit: F,
    ) -> Result<(), smoltcp::phy::IpOutputError>
    where
        F: FnOnce(&mut [u8]),
    {
        self.0
            .consume_full_ip(len, meta, class, ipv4_fragment_ident, emit)
    }

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

/// Supplies type-level Device bounds for local delivery only. Namespace TCP
/// output is routed through its own queue and never transmitted by this device.
struct TransportOnlyDevice;

impl SmolDevice for TransportOnlyDevice {
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

impl SmolDevice for TcpOutputDevice<'_> {
    fn policy_current(&self) -> bool {
        self.policy.policy_current()
    }

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
        // process_tcp_ingress reserves this token before touching TCP state.
        // A stale ruleset therefore asks its caller to requeue the segment.
        if !self.policy.policy_current() {
            return None;
        }
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
    let mut config = smoltcp::iface::Config::new(smoltcp::wire::HardwareAddress::Ip);
    config.random_seed = crate::arch::rand::rand() as u64;
    // The root namespace is constructed by sysfs before timekeeping starts.
    // This empty transport context has no timers; connect/poll supplies the
    // current monotonic time before any protocol state becomes active.
    smoltcp::iface::Interface::new(
        config,
        &mut TransportOnlyDevice,
        smoltcp::time::Instant::ZERO,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_local_retry_has_deadline_without_immediate_poll() {
        let queue = TcpOutputQueue::new();
        let mut reservation = queue.queue.reserve_output().unwrap();
        let frame = alloc::vec![0u8; 20];
        assert!(reservation.try_resize(frame.capacity()));
        let retry_at: smoltcp::time::Instant = crate::time::Instant::now().into();
        let retry_at = retry_at + smoltcp::time::Duration::from_millis(500);
        reservation.requeue_backpressured(
            LocalOutputPacket {
                medium: smoltcp::phy::Medium::Ip,
                meta: PacketMeta::default(),
                disposition: LocalOutputDisposition::Local {
                    oif: 1,
                    ip_mtu: 1500,
                },
                frame,
                ct_context: OutputCtContext::Untracked,
                mark: 0,
                prepared_ip: None,
                _charge: None,
            },
            retry_at,
        );
        let state = queue.poll_state();
        assert!(!state.immediate);
        assert_eq!(state.retry_at, Some(retry_at));
        assert!(queue.queue.release_backpressured_outputs());
        let state = queue.poll_state();
        assert!(state.immediate);
        assert_eq!(state.retry_at, None);
    }
}
