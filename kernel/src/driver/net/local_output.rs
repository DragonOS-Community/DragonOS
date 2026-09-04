use super::*;

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum IfacePollScope {
    None,
    LocalOnly,
    Full,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) enum LocalOutputDrainState {
    Quiescent,
    BudgetExhausted,
    Backpressured,
    Contended,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(super) struct LocalOutputDrainResult {
    pub(super) work_done: usize,
    state: LocalOutputDrainState,
}

impl LocalOutputDrainResult {
    pub(super) const fn new(work_done: usize, state: LocalOutputDrainState) -> Self {
        Self { work_done, state }
    }

    pub(super) fn needs_immediate_poll(self) -> bool {
        matches!(self.state, LocalOutputDrainState::BudgetExhausted)
    }
}

/// Preserve a frame produced by a physical RX/TX token when the device queue
/// cannot accept it immediately. smoltcp consumes such tokens without an
/// error return, so ownership must move into the bounded interface output
/// queue before `consume` returns.
pub(super) fn defer_native_output_after_tx_backpressure(
    iface: &dyn Iface,
    medium: smoltcp::phy::Medium,
    meta: PacketMeta,
    frame: Vec<u8>,
    observed_generation: u64,
) -> Result<(), Vec<u8>> {
    let common = iface.common();
    let Some(mut reservation) = common.local_input_queue.reserve_output() else {
        return Err(frame);
    };
    if !reservation.try_resize(frame.capacity()) {
        return Err(frame);
    }
    let now: smoltcp::time::Instant = crate::time::Instant::now().into();
    let retry_at =
        now + smoltcp::time::Duration::from_micros(common.next_local_output_tx_backoff_us());
    reservation.requeue_native_backpressured(medium, meta, frame, retry_at);
    let retry_at = if common.release_tx_backpressure_after(observed_generation) {
        now
    } else {
        retry_at
    };
    common.schedule_registered_local_output(retry_at);
    Ok(())
}

/// A namespace-local view over the target interface's transport stack.
/// Ingress retains the physical ifindex. Output is staged until the smoltcp
/// locks are released: IPv4 may then select another device through the
/// namespace FIB, while native same-interface and non-IPv4 traffic keeps the
/// underlying device path.
pub(super) struct LocalInputDevice<'a, D: SmolDevice + ?Sized> {
    pub(super) device: &'a mut D,
    pub(super) common: &'a IfaceCommon,
    pub(super) backend_policy: OutputBackendPolicy<'a>,
}

/// Delegates receive to the physical device while routing every response and
/// standalone IPv4 transmission through the same deferred output FIFO as
/// namespace-local input.
pub(super) struct RoutedTxDevice<'a, D: SmolDevice + ?Sized> {
    pub(super) device: &'a mut D,
    pub(super) queue: &'a LocalInputQueue,
    pub(super) backend_policy: OutputBackendPolicy<'a>,
}

/// A physical transmit token with a lazily admitted namespace-routed fallback.
///
/// The physical token remains the common path. `LocalInputTxToken` is used
/// only when the authoritative FIB selects an egress that the owner's native
/// smoltcp projection cannot represent.
pub(super) struct RoutedTxToken<'a, T: SmolTxToken> {
    pub(super) physical: Option<T>,
    pub(super) routed: Option<LocalInputTxToken<'a>>,
    pub(super) queue: &'a LocalInputQueue,
    pub(super) backend_policy: OutputBackendPolicy<'a>,
    pub(super) capabilities: DeviceCapabilities,
}

#[derive(Clone, Copy)]
pub(super) enum OutputBackendDecision {
    NativeOwner,
    Deferred(Option<crate::net::route::OutputRouteDecision>),
}

#[derive(Clone, Copy)]
pub(super) struct OutputBackendPolicy<'a> {
    pub(super) routes: &'a crate::net::route::OutputRouteGuard<'a>,
    pub(super) configured_neighbors: Option<&'a crate::net::neighbor::NeighborReadGuard<'a>>,
    pub(super) owner_ifindex: u32,
    pub(super) owner_is_up: bool,
    pub(super) authoritative_ipv4_output: bool,
}

impl OutputBackendPolicy<'_> {
    pub(super) fn classify(
        self,
        version: smoltcp::wire::IpVersion,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
    ) -> OutputBackendDecision {
        if version != smoltcp::wire::IpVersion::Ipv4 {
            return OutputBackendDecision::NativeOwner;
        }
        let constrained_oif = (meta.id != 0).then_some(meta.id);
        match self.routes.lookup(destination, constrained_oif) {
            Some(route) if route.kind == crate::net::route::RTN_LOCAL => {
                OutputBackendDecision::Deferred(Some(route))
            }
            Some(route)
                if route.oif == self.owner_ifindex
                    && self.owner_is_up
                    && !self.configured_neighbors.is_some_and(|neighbors| {
                        neighbors.lookup(route.oif, route.next_hop).is_some()
                    })
                    && (!self.authoritative_ipv4_output
                        || route.table != crate::net::route::RT_TABLE_DEFAULT) =>
            {
                OutputBackendDecision::NativeOwner
            }
            route => OutputBackendDecision::Deferred(route),
        }
    }
}

/// An owned response buffer temporarily checked out from an interface-local
/// pool. Pool locking is limited to checkout/return; smoltcp and driver
/// callbacks never run while holding it.
pub(super) struct LocalInputScratch<'a> {
    pub(super) buffer: Option<Vec<u8>>,
    pub(super) pool: &'a SpinLock<LocalOutputScratchPool>,
}

impl<'a> LocalInputScratch<'a> {
    pub(super) fn checkout(
        pool: &'a SpinLock<LocalOutputScratchPool>,
        capacity: usize,
    ) -> Option<Self> {
        let mut buffer = {
            let mut pooled = pool.lock();
            let buffer = pooled.buffers.pop().unwrap_or_default();
            pooled.bytes -= buffer.capacity();
            buffer
        };
        buffer.clear();
        if buffer.try_reserve_exact(capacity).is_err() {
            LocalInputQueue::recycle_scratch(pool, buffer);
            return None;
        }
        Some(Self {
            buffer: Some(buffer),
            pool,
        })
    }

    pub(super) fn resize(&mut self, len: usize) -> &mut [u8] {
        let buffer = self
            .buffer
            .as_mut()
            .expect("checked-out scratch always owns its buffer");
        debug_assert!(len <= buffer.capacity());
        buffer.resize(len, 0);
        buffer.as_mut_slice()
    }

    fn try_ensure_capacity(
        &mut self,
        capacity: usize,
        reservation: &mut LocalOutputReservation<'_>,
    ) -> bool {
        let buffer = self
            .buffer
            .as_mut()
            .expect("checked-out scratch always owns its buffer");
        if buffer.capacity() >= capacity {
            return true;
        }
        debug_assert!(buffer.is_empty());
        let mut replacement = Vec::new();
        if replacement.try_reserve_exact(capacity).is_err()
            || !reservation.try_resize(replacement.capacity())
        {
            return false;
        }
        core::mem::swap(buffer, &mut replacement);
        LocalInputQueue::recycle_scratch(self.pool, replacement);
        true
    }

    pub(super) fn capacity(&self) -> usize {
        self.buffer.as_ref().map_or(0, Vec::capacity)
    }

    pub(super) fn take(&mut self) -> Option<Vec<u8>> {
        self.buffer.take()
    }
}

impl Drop for LocalInputScratch<'_> {
    fn drop(&mut self) {
        let Some(buffer) = self.buffer.take() else {
            return;
        };
        LocalInputQueue::recycle_scratch(self.pool, buffer);
    }
}

/// A response token paired with namespace-local ingress.
///
/// A local-stack response is completed in memory and then sent through the
/// namespace FIB. This keeps transport progress independent of the address
/// owner's physical TX queue and lets output select its own interface.
pub(super) struct LocalInputTxToken<'a> {
    pub(super) medium: smoltcp::phy::Medium,
    pub(super) meta: PacketMeta,
    pub(super) disposition: LocalOutputDisposition,
    pub(super) backend_policy: OutputBackendPolicy<'a>,
    pub(super) owner_ip_mtu: usize,
    pub(super) reservation: LocalOutputReservation<'a>,
    pub(super) scratch: LocalInputScratch<'a>,
}

impl SmolTxToken for LocalInputTxToken<'_> {
    fn egress_override(
        &mut self,
        version: smoltcp::wire::IpVersion,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
    ) -> Result<Option<smoltcp::phy::TxEgressOverride>, smoltcp::phy::TxEgressError> {
        let decision = self.backend_policy.classify(version, destination, meta);
        self.apply_backend_decision(decision)
    }

    fn apply_egress_override(
        &mut self,
        egress: Option<smoltcp::phy::TxEgressOverride>,
    ) -> Result<(), smoltcp::phy::TxEgressError> {
        let Some(egress) = egress else {
            self.disposition = LocalOutputDisposition::NativeOwner;
            return Ok(());
        };
        if !self
            .scratch
            .try_ensure_capacity(egress.ip_mtu, &mut self.reservation)
        {
            self.disposition = LocalOutputDisposition::Drop;
            return Err(smoltcp::phy::TxEgressError::Exhausted);
        }
        self.medium = egress.medium;
        self.disposition = LocalOutputDisposition::from_context(egress.context, egress.ip_mtu);
        Ok(())
    }

    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let Self {
            medium,
            meta,
            disposition,
            backend_policy: _,
            owner_ip_mtu: _,
            reservation,
            mut scratch,
        } = self;
        let result = f(scratch.resize(len));
        reservation.commit(medium, meta, disposition, &mut scratch);
        result
    }

    fn set_meta(&mut self, meta: PacketMeta) {
        self.meta = meta;
    }
}

impl LocalInputTxToken<'_> {
    pub(super) fn apply_backend_decision(
        &mut self,
        decision: OutputBackendDecision,
    ) -> Result<Option<smoltcp::phy::TxEgressOverride>, smoltcp::phy::TxEgressError> {
        let OutputBackendDecision::Deferred(route) = decision else {
            return Ok(Some(smoltcp::phy::TxEgressOverride {
                medium: self.medium,
                ip_mtu: self.owner_ip_mtu,
                context: LocalOutputDisposition::NATIVE_CONTEXT,
            }));
        };
        if let Some(route) = route {
            if route.kind == crate::net::route::RTN_LOCAL {
                let route_ip_mtu = core::cmp::min(route.ip_mtu, u16::MAX as usize);
                let ip_mtu = if self
                    .scratch
                    .try_ensure_capacity(route_ip_mtu, &mut self.reservation)
                {
                    route_ip_mtu
                } else {
                    core::cmp::min(route_ip_mtu, self.owner_ip_mtu)
                };
                self.medium = smoltcp::phy::Medium::Ip;
                self.disposition = LocalOutputDisposition::Local {
                    oif: route.oif,
                    ip_mtu,
                };
                return Ok(Some(smoltcp::phy::TxEgressOverride {
                    medium: smoltcp::phy::Medium::Ip,
                    ip_mtu,
                    context: LocalOutputDisposition::local_context(route.oif),
                }));
            }
            let smoltcp::wire::IpAddress::Ipv4(next_hop) = route.next_hop else {
                self.medium = smoltcp::phy::Medium::Ip;
                self.disposition = LocalOutputDisposition::Drop;
                return Ok(Some(smoltcp::phy::TxEgressOverride {
                    medium: smoltcp::phy::Medium::Ip,
                    ip_mtu: self.owner_ip_mtu,
                    context: LocalOutputDisposition::DROP_CONTEXT,
                }));
            };
            self.medium = smoltcp::phy::Medium::Ip;
            // The address owner and selected egress may have different MTUs.
            // Grow only the token that actually needs the larger route MTU;
            // on allocation pressure, a smaller legal fragment/drop boundary
            // is preferable to constructing beyond the scratch capacity.
            // IPv4's total-length field is the hard protocol ceiling even if
            // userspace configured a larger logical device MTU.
            let route_ip_mtu = core::cmp::min(route.ip_mtu, u16::MAX as usize);
            let ip_mtu = if self
                .scratch
                .try_ensure_capacity(route_ip_mtu, &mut self.reservation)
            {
                route_ip_mtu
            } else {
                core::cmp::min(route_ip_mtu, self.owner_ip_mtu)
            };
            self.disposition = LocalOutputDisposition::Routed {
                oif: route.oif,
                next_hop,
                ip_mtu,
            };
            return Ok(Some(smoltcp::phy::TxEgressOverride {
                medium: smoltcp::phy::Medium::Ip,
                ip_mtu,
                context: LocalOutputDisposition::routed_context(route.oif, next_hop),
            }));
        }
        self.medium = smoltcp::phy::Medium::Ip;
        self.disposition = LocalOutputDisposition::Drop;
        Ok(Some(smoltcp::phy::TxEgressOverride {
            medium: smoltcp::phy::Medium::Ip,
            ip_mtu: self.owner_ip_mtu,
            context: LocalOutputDisposition::DROP_CONTEXT,
        }))
    }
}

impl<T: SmolTxToken> SmolTxToken for RoutedTxToken<'_, T> {
    fn egress_override(
        &mut self,
        version: smoltcp::wire::IpVersion,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
    ) -> Result<Option<smoltcp::phy::TxEgressOverride>, smoltcp::phy::TxEgressError> {
        let decision = self.backend_policy.classify(version, destination, meta);
        if matches!(decision, OutputBackendDecision::NativeOwner) {
            if self.physical.is_none() {
                return Err(smoltcp::phy::TxEgressError::Exhausted);
            }
            return Ok(Some(smoltcp::phy::TxEgressOverride {
                medium: self.capabilities.medium,
                ip_mtu: self.capabilities.ip_mtu(),
                context: LocalOutputDisposition::NATIVE_CONTEXT,
            }));
        }
        let mut routed = local_tx_token(self.queue, self.backend_policy, self.capabilities.clone())
            .ok_or(smoltcp::phy::TxEgressError::Exhausted)?;
        let override_ = routed.apply_backend_decision(decision)?;
        self.routed = Some(routed);
        Ok(override_)
    }

    fn apply_egress_override(
        &mut self,
        egress: Option<smoltcp::phy::TxEgressOverride>,
    ) -> Result<(), smoltcp::phy::TxEgressError> {
        let Some(egress) = egress else {
            return self
                .physical
                .as_ref()
                .map(|_| ())
                .ok_or(smoltcp::phy::TxEgressError::Exhausted);
        };
        if egress.context == LocalOutputDisposition::NATIVE_CONTEXT {
            return self
                .physical
                .as_ref()
                .map(|_| ())
                .ok_or(smoltcp::phy::TxEgressError::Exhausted);
        }
        let mut routed = local_tx_token(self.queue, self.backend_policy, self.capabilities.clone())
            .ok_or(smoltcp::phy::TxEgressError::Exhausted)?;
        routed.apply_egress_override(Some(egress))?;
        self.routed = Some(routed);
        Ok(())
    }

    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        match (self.routed, self.physical) {
            (Some(routed), _) => routed.consume(len, f),
            (None, Some(physical)) => physical.consume(len, f),
            (None, None) => unreachable!("egress admission selected no transmit backend"),
        }
    }

    fn set_meta(&mut self, meta: PacketMeta) {
        if let Some(physical) = self.physical.as_mut() {
            physical.set_meta(meta);
        }
        if let Some(routed) = self.routed.as_mut() {
            routed.set_meta(meta);
        }
    }
}

impl<'a, D: SmolDevice + ?Sized> LocalInputDevice<'a, D> {
    pub(super) fn new(
        device: &'a mut D,
        common: &'a IfaceCommon,
        backend_policy: OutputBackendPolicy<'a>,
    ) -> Self {
        Self {
            device,
            common,
            backend_policy,
        }
    }

    pub(super) fn tx_token(&self) -> Option<LocalInputTxToken<'a>> {
        local_tx_token(
            &self.common.local_input_queue,
            self.backend_policy,
            self.device.capabilities(),
        )
    }
}

pub(super) fn local_tx_token<'a>(
    queue: &'a LocalInputQueue,
    backend_policy: OutputBackendPolicy<'a>,
    capabilities: DeviceCapabilities,
) -> Option<LocalInputTxToken<'a>> {
    let mut reservation = queue.reserve_output()?;
    let scratch =
        LocalInputScratch::checkout(&queue.response_scratch, capabilities.max_transmission_unit)?;
    if !reservation.try_resize(scratch.capacity()) {
        return None;
    }
    Some(LocalInputTxToken {
        medium: capabilities.medium,
        meta: PacketMeta::default(),
        disposition: LocalOutputDisposition::NativeOwner,
        backend_policy,
        owner_ip_mtu: capabilities.ip_mtu(),
        reservation,
        scratch,
    })
}

impl<D: SmolDevice + ?Sized> SmolDevice for LocalInputDevice<'_, D> {
    type RxToken<'a>
        = LocalInputRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = LocalInputTxToken<'a>
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        // Reserve response capacity before consuming ingress. If the bounded
        // output queue is full, smoltcp observes device backpressure and the
        // input remains queued for a later poll.
        let tx_token = self.tx_token()?;
        let packet = self.common.local_input_queue.pop()?;
        // Namespace-local delivery is an ingress path in its own right.
        // Apply the same pre-stack policy as a driver receive queue so a
        // routed local packet cannot bypass listener/backlog semantics. Stop
        // this ingress round after one policy drop to keep NAPI work bounded;
        // the non-empty local queue schedules the next round.
        if self.common.should_drop_rx_packet(&packet.ip_packet) {
            return None;
        }
        let ingress_ifindex = packet.ingress_ifindex;
        let frame = packet.into_frame(self.device.capabilities().medium).ok()?;
        let mut meta = PacketMeta::default();
        meta.id = ingress_ifindex;
        Some((LocalInputRxToken { frame, meta }, tx_token))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        self.tx_token()
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
}

impl<D: SmolDevice + ?Sized> SmolDevice for RoutedTxDevice<'_, D> {
    type RxToken<'a>
        = D::RxToken<'a>
    where
        Self: 'a;
    type TxToken<'a>
        = RoutedTxToken<'a, D::TxToken<'a>>
    where
        Self: 'a;

    fn receive(
        &mut self,
        timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let capabilities = self.device.capabilities();
        let (rx_token, physical) = self.device.receive(timestamp)?;
        Some((
            rx_token,
            RoutedTxToken {
                physical: Some(physical),
                routed: None,
                queue: self.queue,
                backend_policy: self.backend_policy,
                capabilities,
            },
        ))
    }

    fn transmit(&mut self, timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let capabilities = self.device.capabilities();
        let physical = self.device.transmit(timestamp);
        Some(RoutedTxToken {
            physical,
            routed: None,
            queue: self.queue,
            backend_policy: self.backend_policy,
            capabilities,
        })
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
}

pub(super) enum LocalOutputTransmitResult {
    Sent(LocalOutputPacket),
    RetrySoon(LocalOutputPacket),
    RetryAt {
        packet: LocalOutputPacket,
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    },
    Drop(LocalOutputPacket, SystemError),
}

pub(super) fn output_error(
    packet: LocalOutputPacket,
    error: SystemError,
) -> LocalOutputTransmitResult {
    match error {
        SystemError::ENOBUFS | SystemError::EAGAIN_OR_EWOULDBLOCK => {
            LocalOutputTransmitResult::RetrySoon(packet)
        }
        _ => LocalOutputTransmitResult::Drop(packet, error),
    }
}

pub(super) fn transmit_routed_stack_output(
    iface: &dyn Iface,
    packet: LocalOutputPacket,
) -> LocalOutputTransmitResult {
    let LocalOutputDisposition::Routed {
        next_hop, ip_mtu, ..
    } = packet.disposition
    else {
        return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
    };
    if packet.medium != smoltcp::phy::Medium::Ip
        || packet.frame.len() > ip_mtu
        || packet.frame.first().map(|byte| byte >> 4) != Some(4)
    {
        return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
    }
    if !iface.flags().contains(InterfaceFlags::UP) || packet.frame.len() > iface.mtu() {
        let error = if !iface.flags().contains(InterfaceFlags::UP) {
            SystemError::ENETDOWN
        } else {
            SystemError::EMSGSIZE
        };
        return LocalOutputTransmitResult::Drop(packet, error);
    }
    match iface.route_and_send(&smoltcp::wire::IpAddress::Ipv4(next_hop), &packet.frame) {
        Ok(()) => LocalOutputTransmitResult::Sent(packet),
        Err(RouteSendError::RetryAt {
            retry_at,
            probe_sent,
        }) => LocalOutputTransmitResult::RetryAt {
            packet,
            retry_at,
            probe_sent,
        },
        Err(RouteSendError::Failed(error)) => output_error(packet, error),
    }
}

/// Transmit a routed packet after the actual egress has reserved queue
/// capacity. Any retry is committed to that egress before the caller releases
/// its source reservation, so TX-completion wakeups always target the owner of
/// the backpressured resource.
pub(super) fn transmit_admitted_routed_output(
    iface: &dyn Iface,
    packet: LocalOutputPacket,
    reservation: LocalOutputReservation<'_>,
) -> AdmittedRoutedOutput {
    let LocalOutputDisposition::Routed { oif, next_hop, .. } = packet.disposition else {
        drop(reservation);
        return AdmittedRoutedOutput::Drop(packet, SystemError::EINVAL);
    };
    let tx_generation = iface.common().tx_completion_generation();
    match transmit_routed_stack_output(iface, packet) {
        LocalOutputTransmitResult::Sent(packet) => {
            iface.common().reset_local_output_tx_backoff();
            iface
                .common()
                .local_input_queue
                .release_neighbor(oif, next_hop);
            drop(reservation);
            AdmittedRoutedOutput::Sent(packet)
        }
        LocalOutputTransmitResult::RetrySoon(packet) => {
            let delay_us = iface.common().next_local_output_tx_backoff_us();
            let now: smoltcp::time::Instant = crate::time::Instant::now().into();
            let retry_at = now + smoltcp::time::Duration::from_micros(delay_us);
            reservation.requeue_backpressured(packet, retry_at);
            let retry_at = if iface.common().release_tx_backpressure_after(tx_generation) {
                now
            } else {
                retry_at
            };
            AdmittedRoutedOutput::Queued(retry_at)
        }
        LocalOutputTransmitResult::RetryAt {
            packet,
            retry_at,
            probe_sent,
        } => {
            iface.common().reset_local_output_tx_backoff();
            match reservation.requeue_deferred(packet, retry_at, probe_sent) {
                Ok(()) => {
                    let released = iface.net_namespace().is_some_and(|netns| {
                        crate::net::neighbor::release_deferred_after_enqueue(
                            &netns,
                            iface.common(),
                            oif,
                            next_hop,
                        )
                    });
                    let retry_at = if released {
                        crate::time::Instant::now().into()
                    } else {
                        retry_at
                    };
                    AdmittedRoutedOutput::Queued(retry_at)
                }
                Err(packet) => AdmittedRoutedOutput::Drop(packet, SystemError::ENOBUFS),
            }
        }
        LocalOutputTransmitResult::Drop(packet, error) => {
            iface.common().reset_local_output_tx_backoff();
            drop(reservation);
            AdmittedRoutedOutput::Drop(packet, error)
        }
    }
}

pub(super) fn transmit_local_stack_output<D>(
    netns: &Arc<NetNamespace>,
    owner_is_up: bool,
    device: &mut D,
    packet: LocalOutputPacket,
) -> LocalOutputTransmitResult
where
    D: SmolDevice + ?Sized,
{
    match packet.disposition {
        LocalOutputDisposition::NativeOwner => {
            transmit_native_output_if_up(device, packet, owner_is_up)
        }
        LocalOutputDisposition::Drop => {
            LocalOutputTransmitResult::Drop(packet, SystemError::ENETUNREACH)
        }
        LocalOutputDisposition::Local { oif, ip_mtu } => {
            if packet.medium != smoltcp::phy::Medium::Ip
                || packet.frame.len() > ip_mtu
                || packet.frame.first().map(|byte| byte >> 4) != Some(4)
            {
                return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
            }
            let Some(iface) = netns.device_list().get(&(oif as usize)).cloned() else {
                return LocalOutputTransmitResult::Drop(packet, SystemError::ENODEV);
            };
            if packet.frame.len() > iface.mtu() {
                return LocalOutputTransmitResult::Drop(packet, SystemError::EMSGSIZE);
            }
            match iface.inject_local_ipv4_packet(oif, iface.mac(), &packet.frame, false) {
                Ok(()) => LocalOutputTransmitResult::Sent(packet),
                // This is receive-backlog congestion, not physical TX
                // backpressure. Linux may drop locally delivered packets when
                // the receive backlog is full; a TX completion cannot make
                // this target input queue writable.
                Err(error) => LocalOutputTransmitResult::Drop(packet, error),
            }
        }
        LocalOutputDisposition::Routed { oif, .. } => {
            let Some(iface) = netns.device_list().get(&(oif as usize)).cloned() else {
                return LocalOutputTransmitResult::Drop(packet, SystemError::ENODEV);
            };
            transmit_routed_stack_output(iface.as_ref(), packet)
        }
    }
}

pub(super) fn transmit_native_output_if_up<D>(
    device: &mut D,
    packet: LocalOutputPacket,
    owner_is_up: bool,
) -> LocalOutputTransmitResult
where
    D: SmolDevice + ?Sized,
{
    if !owner_is_up {
        return LocalOutputTransmitResult::Drop(packet, SystemError::ENETDOWN);
    }
    transmit_native_output(device, packet)
}

pub(super) fn transmit_native_output<D>(
    device: &mut D,
    packet: LocalOutputPacket,
) -> LocalOutputTransmitResult
where
    D: SmolDevice + ?Sized,
{
    let Some(mut token) = device.transmit(crate::time::Instant::now().into()) else {
        return LocalOutputTransmitResult::RetrySoon(packet);
    };
    token.set_meta(packet.meta);
    token.consume(packet.frame.len(), |buffer| {
        buffer.copy_from_slice(&packet.frame);
    });
    LocalOutputTransmitResult::Sent(packet)
}
