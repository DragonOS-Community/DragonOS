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
/// locks are released: unicast IP may then select another device through the
/// namespace FIB, while native link-local control traffic keeps the device path.
pub(super) struct LocalInputDevice<'a, D: SmolDevice + ?Sized> {
    pub(super) device: &'a mut D,
    pub(super) common: &'a IfaceCommon,
    pub(super) backend_policy: OutputBackendPolicy<'a>,
    pub(super) stage_cell: Option<&'a Cell<IngressStage>>,
    pub(super) broadcast_cell: Option<&'a Cell<bool>>,
    pub(super) ct_context_cell:
        Option<&'a core::cell::RefCell<Option<crate::net::conntrack::CtPacketContext>>>,
    pub(super) mark_cell: Option<&'a Cell<u32>>,
    pub(super) forward_fragment_cell:
        Option<&'a Cell<Option<crate::net::forward_mtu::ReassembledForwardInfo>>>,
}

/// Delegates receive to the physical device while routing every response and
/// standalone routed IP transmission through the same deferred output FIFO as
/// namespace-local input.
pub(super) struct RoutedTxDevice<'a, D: SmolDevice + ?Sized> {
    pub(super) device: &'a mut D,
    pub(super) queue: &'a LocalInputQueue,
    pub(super) backend_policy: OutputBackendPolicy<'a>,
}

/// Metadata describes the physical ingress, not the TCP SocketSet owner.
pub(super) struct RoutedRxToken<T> {
    inner: T,
    ifindex: u32,
}

impl<T: RxToken> RxToken for RoutedRxToken<T> {
    fn consume<R, F: FnOnce(&[u8]) -> R>(self, f: F) -> R {
        self.inner.consume(f)
    }

    fn meta(&self) -> PacketMeta {
        let mut meta = self.inner.meta();
        meta.id = self.ifindex;
        meta
    }
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
    pub(super) netns: &'a NetNamespace,
    pub(super) routes: &'a crate::net::route::OutputRouteGuard<'a>,
    pub(super) ruleset: Option<&'a crate::net::nftables::RulesetSnapshot>,
    pub(super) device_names: &'a crate::net::nftables::NftDeviceNames,
    pub(super) configured_neighbors: Option<&'a crate::net::neighbor::NeighborReadGuard<'a>>,
    pub(super) owner_ifindex: u32,
    pub(super) owner_is_up: bool,
    pub(super) authoritative_output: bool,
}

impl OutputBackendPolicy<'_> {
    pub(super) fn policy_current(self) -> bool {
        self.ruleset
            .is_some_and(|ruleset| ruleset.generation == self.netns.nftables().generation())
    }

    fn requires_output_admission(self, version: smoltcp::wire::IpVersion) -> bool {
        self.ruleset.is_some_and(|ruleset| {
            if ruleset.conntrack_registered(version) {
                return true;
            }
            let has_hook = |hook| match version {
                smoltcp::wire::IpVersion::Ipv4 => ruleset.has_ipv4_hook(hook),
                smoltcp::wire::IpVersion::Ipv6 => ruleset.has_ipv6_hook(hook),
            };
            has_hook(crate::net::nftables::NftIpv4Hook::LocalOut)
                || has_hook(crate::net::nftables::NftIpv4Hook::PostRouting)
        })
    }

    pub(super) fn hook_oifname(
        self,
        hook: crate::net::nftables::NftIpv4Hook,
        version: smoltcp::wire::IpVersion,
        oif: u32,
    ) -> Option<[u8; 16]> {
        let ruleset = self.ruleset?;
        let needed = match version {
            smoltcp::wire::IpVersion::Ipv4 => ruleset.hook_requires_iface_names(hook),
            smoltcp::wire::IpVersion::Ipv6 => ruleset.ipv6_hook_requires_iface_names(hook),
        };
        if needed {
            self.device_names.get(oif)
        } else {
            Some([0; 16])
        }
    }

    pub(super) fn outbound_ip_mtu(
        self,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
        native_mtu: usize,
    ) -> usize {
        match self.classify(destination.version(), destination, meta) {
            OutputBackendDecision::Deferred(Some(route)) => route.ip_mtu.min(u16::MAX as usize),
            // A missing route must not prevent TCP from advancing its timers.
            // Actual packet dispatch still rejects the missing route.
            _ => native_mtu,
        }
    }

    pub(super) fn classify(
        self,
        version: smoltcp::wire::IpVersion,
        destination: smoltcp::wire::IpAddress,
        meta: PacketMeta,
    ) -> OutputBackendDecision {
        if self.owner_ifindex != 0
            && version == smoltcp::wire::IpVersion::Ipv6
            && destination.is_multicast()
        {
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
                    && (!self.authoritative_output
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
    fn take_pooled(
        pool: &SpinLock<LocalOutputScratchPool>,
        min_capacity: usize,
        max_capacity: usize,
    ) -> Vec<u8> {
        let mut pooled = pool.lock();
        // The pool is bounded to 64 buffers. Pick a compatible size so a
        // jumbo route does not force small packets to reserve jumbo storage,
        // while the next jumbo packet can reuse its own returned buffer.
        let Some(index) = pooled
            .buffers
            .iter()
            .rposition(|buffer| (min_capacity..=max_capacity).contains(&buffer.capacity()))
        else {
            return Vec::new();
        };
        let buffer = pooled.buffers.swap_remove(index);
        pooled.bytes -= buffer.capacity();
        buffer
    }

    pub(super) fn checkout(
        pool: &'a SpinLock<LocalOutputScratchPool>,
        capacity: usize,
    ) -> Option<Self> {
        let mut buffer = Self::take_pooled(pool, 0, capacity);
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
        let mut replacement = Self::take_pooled(self.pool, capacity, capacity + capacity / 4);
        if replacement.try_reserve_exact(capacity).is_err()
            || !reservation.try_resize(replacement.capacity())
        {
            LocalInputQueue::recycle_scratch(self.pool, replacement);
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

/// Owns one complete IP datagram and the source interface's bounded TX
/// capacity before any OUTPUT/POST_ROUTING side effect is run. Dropping it
/// releases both resources without publishing a partial packet.
pub(crate) struct PreparedIpOutputReservation<'a> {
    common: &'a IfaceCommon,
    expected_netns: Arc<crate::process::namespace::net_namespace::NetNamespace>,
    owner_epoch: u64,
    reservation: LocalOutputReservation<'a>,
    scratch: LocalInputScratch<'a>,
    len: usize,
    version: smoltcp::wire::IpVersion,
    charge: Option<OutputCharge>,
}

pub(crate) fn reserve_prepared_ip_output<'a>(
    source: &'a dyn Iface,
    netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
    len: usize,
    version: smoltcp::wire::IpVersion,
) -> Result<PreparedIpOutputReservation<'a>, SystemError> {
    if !source
        .net_namespace()
        .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
    {
        return Err(SystemError::ENODEV);
    }
    PreparedIpOutputReservation::reserve(source.common(), netns, len, version)
}

impl<'a> PreparedIpOutputReservation<'a> {
    pub(super) fn reserve(
        common: &'a IfaceCommon,
        netns: &Arc<crate::process::namespace::net_namespace::NetNamespace>,
        len: usize,
        version: smoltcp::wire::IpVersion,
    ) -> Result<Self, SystemError> {
        let owner_epoch = common.namespace_epoch();
        if owner_epoch & 1 != 0 {
            return Err(SystemError::ENODEV);
        }
        let valid = match version {
            smoltcp::wire::IpVersion::Ipv4 => (20..=u16::MAX as usize).contains(&len),
            smoltcp::wire::IpVersion::Ipv6 => (40..=40 + u16::MAX as usize).contains(&len),
        };
        if !valid {
            return Err(SystemError::EMSGSIZE);
        }
        let mut reservation = common
            .local_input_queue
            .reserve_output()
            .ok_or(SystemError::ENOBUFS)?;
        let mut scratch =
            LocalInputScratch::checkout(&common.local_input_queue.response_scratch, len)
                .ok_or(SystemError::ENOMEM)?;
        if !reservation.try_resize(scratch.capacity()) {
            return Err(SystemError::ENOBUFS);
        }
        scratch.resize(len);
        if common.namespace_epoch() != owner_epoch
            || !common
                .net_namespace()
                .is_some_and(|owner| Arc::ptr_eq(&owner, netns))
        {
            return Err(SystemError::ENODEV);
        }
        Ok(Self {
            common,
            expected_netns: netns.clone(),
            owner_epoch,
            reservation,
            scratch,
            len,
            version,
            charge: None,
        })
    }

    pub(crate) fn set_charge(&mut self, charge: OutputCharge) {
        debug_assert!(self.charge.is_none());
        self.charge = Some(charge);
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        self.scratch.buffer.as_ref().unwrap().as_slice()
    }

    pub(crate) fn bytes_mut(&mut self) -> &mut [u8] {
        self.scratch.buffer.as_mut().unwrap().as_mut_slice()
    }

    /// Validate the complete datagram and the selected route before any
    /// OUTPUT/POST_ROUTING hook or local multicast copy can observe it.
    pub(crate) fn validate_for_route(
        &self,
        route: crate::net::route::OutputRouteDecision,
        may_fragment: bool,
    ) -> Result<(), SystemError> {
        if self.version != smoltcp::wire::IpVersion::Ipv4 {
            return Err(SystemError::EINVAL);
        }
        if !matches!(route.next_hop, smoltcp::wire::IpAddress::Ipv4(_)) {
            return Err(SystemError::EINVAL);
        }
        // Loopback may advertise 64 KiB, larger than the maximum IPv4 packet.
        if route.oif == 0 || route.ip_mtu < 68 {
            return Err(SystemError::EINVAL);
        }
        if !matches!(
            route.kind,
            crate::net::route::RTN_LOCAL
                | crate::net::route::RTN_UNICAST
                | crate::net::route::RTN_BROADCAST
                | crate::net::route::RTN_MULTICAST
        ) {
            return Err(SystemError::ENETUNREACH);
        }
        let parsed = ParsedIpv4Output::parse(self.bytes())?;
        if self.len > route.ip_mtu
            && (!may_fragment || parsed.df || route.ip_mtu < parsed.header_len + 8)
        {
            return Err(SystemError::EMSGSIZE);
        }
        Ok(())
    }

    /// Caller owns the policy decision to permit fragmentation. In particular
    /// raw IP_HDRINCL supplies `false`; this layer never clears DF on its own.
    pub(crate) fn commit(
        mut self,
        route: crate::net::route::OutputRouteDecision,
        may_fragment: bool,
        ct_context: OutputCtContext,
        mark: u32,
    ) -> Result<(), SystemError> {
        self.check_owner()?;
        self.validate_for_route(route, may_fragment)?;
        let smoltcp::wire::IpAddress::Ipv4(next_hop) = route.next_hop else {
            return Err(SystemError::EINVAL);
        };
        let disposition = if route.kind == crate::net::route::RTN_LOCAL {
            LocalOutputDisposition::Local {
                oif: route.oif,
                ip_mtu: route.ip_mtu,
            }
        } else {
            LocalOutputDisposition::Routed {
                oif: route.oif,
                next_hop: next_hop.into(),
                ip_mtu: route.ip_mtu,
            }
        };
        let frame = self.scratch.take().unwrap();
        self.reservation.commit_prepared_ipv4(
            frame,
            PacketMeta::default(),
            disposition,
            may_fragment,
            self.charge.take(),
            ct_context,
            mark,
        );
        self.common
            .schedule_registered_local_output(crate::time::Instant::now().into());
        Ok(())
    }

    pub(crate) fn validate_for_ipv6_route(
        &self,
        route: crate::net::route::OutputRouteDecision,
    ) -> Result<(), SystemError> {
        if self.version != smoltcp::wire::IpVersion::Ipv6
            || !matches!(route.next_hop, smoltcp::wire::IpAddress::Ipv6(_))
            || route.oif == 0
        {
            return Err(SystemError::EINVAL);
        }
        if !matches!(
            route.kind,
            crate::net::route::RTN_LOCAL
                | crate::net::route::RTN_UNICAST
                | crate::net::route::RTN_MULTICAST
        ) {
            return Err(SystemError::ENETUNREACH);
        }
        let packet = smoltcp::wire::Ipv6Packet::new_checked(self.bytes())
            .map_err(|_| SystemError::EINVAL)?;
        if packet.total_len() != self.len {
            return Err(SystemError::EINVAL);
        }
        // The fragmenter inserts immediately after the fixed IPv6 header.
        // Packets with extension headers can still be sent intact, but need
        // a separate unfragmentable-header walk before source fragmentation.
        if self.len > route.ip_mtu
            && (route.ip_mtu < 56
                || matches!(u8::from(packet.next_header()), 0 | 43 | 44 | 51 | 60))
        {
            return Err(SystemError::EMSGSIZE);
        }
        Ok(())
    }

    pub(crate) fn commit_ipv6(
        mut self,
        route: crate::net::route::OutputRouteDecision,
        identification: u32,
        ct_context: OutputCtContext,
        mark: u32,
    ) -> Result<(), SystemError> {
        self.check_owner()?;
        self.validate_for_ipv6_route(route)?;
        let disposition = if route.kind == crate::net::route::RTN_LOCAL {
            LocalOutputDisposition::Local {
                oif: route.oif,
                ip_mtu: route.ip_mtu,
            }
        } else {
            LocalOutputDisposition::Routed {
                oif: route.oif,
                next_hop: route.next_hop,
                ip_mtu: route.ip_mtu,
            }
        };
        self.reservation.commit_prepared_ipv6(
            self.scratch.take().unwrap(),
            PacketMeta::default(),
            disposition,
            identification,
            self.charge.take(),
            ct_context,
            mark,
        );
        self.common
            .schedule_registered_local_output(crate::time::Instant::now().into());
        Ok(())
    }

    fn check_owner(&self) -> Result<(), SystemError> {
        if self.common.namespace_epoch() != self.owner_epoch
            || !self
                .common
                .net_namespace()
                .is_some_and(|owner| Arc::ptr_eq(&owner, &self.expected_netns))
        {
            return Err(SystemError::ENODEV);
        }
        Ok(())
    }
}

/// Parsed once at admission and again before each fragment so a changing MTU
/// can be honored without altering the admitted datagram.
struct ParsedIpv4Output {
    header_len: usize,
    payload_len: usize,
    df: bool,
    frag_offset_bytes: usize,
    more_fragments: bool,
}

impl ParsedIpv4Output {
    fn parse(bytes: &[u8]) -> Result<Self, SystemError> {
        if bytes.len() < 20 || bytes[0] >> 4 != 4 {
            return Err(SystemError::EINVAL);
        }
        let header_len = ((bytes[0] & 0x0f) as usize) * 4;
        if !(20..=60).contains(&header_len)
            || header_len > bytes.len()
            || usize::from(u16::from_be_bytes([bytes[2], bytes[3]])) != bytes.len()
        {
            return Err(SystemError::EINVAL);
        }
        let flags = u16::from_be_bytes([bytes[6], bytes[7]]);
        let frag_offset_bytes = ((flags & 0x1fff) as usize) * 8;
        let payload_len = bytes.len() - header_len;
        if frag_offset_bytes
            .checked_add(payload_len)
            .is_none_or(|len| len > u16::MAX as usize)
            || flags & 0x2000 != 0 && !payload_len.is_multiple_of(8)
        {
            return Err(SystemError::EINVAL);
        }
        // Invalid option lengths cannot be deferred to a partially-sent
        // fragmentation sequence. The copied bit is interpreted at emit.
        let mut option = 20;
        while option < header_len {
            match bytes[option] {
                0 => break,
                1 => option += 1,
                _ => {
                    if option + 2 > header_len {
                        return Err(SystemError::EINVAL);
                    }
                    let len = bytes[option + 1] as usize;
                    if len < 2 || option + len > header_len {
                        return Err(SystemError::EINVAL);
                    }
                    option += len;
                }
            }
        }
        Ok(Self {
            header_len,
            payload_len,
            df: flags & 0x4000 != 0,
            frag_offset_bytes,
            more_fragments: flags & 0x2000 != 0,
        })
    }
}

/// A forwarded IPv4 datagram has already passed FORWARD and POST_ROUTING.
/// Preserve its fragmentability through the bounded output queue, including
/// when the egress MTU changes after admission.
pub(super) fn routed_ipv4_progress(
    bytes: &[u8],
    reassembled: Option<crate::net::forward_mtu::ReassembledForwardInfo>,
) -> Result<PreparedIpProgress, SystemError> {
    let parsed = ParsedIpv4Output::parse(bytes)?;
    Ok(PreparedIpProgress::Ipv4 {
        offset: 0,
        may_fragment: !parsed.df || reassembled.is_some(),
        max_fragment_len: reassembled.map(|info| info.max_original_fragment_len),
    })
}

/// smoltcp emits an unsplit IPv4 header with DF=1 and ID=0. Its native
/// fragment path would replace both fields for an oversized generated packet;
/// deferred output must make the same decision before queuing the datagram.
fn prepare_deferred_ipv4_for_mtu(
    bytes: &mut [u8],
    ip_mtu: usize,
    fragment_ident: Option<u16>,
) -> Result<bool, smoltcp::phy::IpOutputError> {
    use smoltcp::phy::IpOutputError;

    let parsed = ParsedIpv4Output::parse(bytes).map_err(|_| IpOutputError::NoRoute)?;
    if bytes.len() <= ip_mtu {
        return Ok(!parsed.df);
    }
    if ip_mtu < parsed.header_len + 8 {
        return Err(IpOutputError::MtuExceeded);
    }
    if parsed.df {
        let ident = fragment_ident.ok_or(IpOutputError::MtuExceeded)?;
        bytes[4..6].copy_from_slice(&ident.to_be_bytes());
        bytes[6] &= !0x40;
        bytes[10..12].fill(0);
        smoltcp::wire::Ipv4Packet::new_unchecked(bytes).fill_checksum();
    }
    Ok(true)
}

fn prepared_ipv4_fragment(
    packet: &LocalOutputPacket,
    mtu: usize,
) -> Result<(Vec<u8>, usize), SystemError> {
    let parsed = ParsedIpv4Output::parse(&packet.frame)?;
    let Some(PreparedIpProgress::Ipv4 { offset, .. }) = packet.prepared_ip else {
        return Err(SystemError::EINVAL);
    };
    if offset >= parsed.payload_len || mtu < parsed.header_len + 8 {
        return Err(SystemError::EMSGSIZE);
    }
    let remaining = parsed.payload_len - offset;
    let available = mtu - parsed.header_len;
    let chunk = if remaining > available {
        available & !7
    } else {
        remaining
    };
    if chunk == 0 || (remaining > chunk && chunk % 8 != 0) {
        return Err(SystemError::EMSGSIZE);
    }
    let start = parsed.frag_offset_bytes + offset;
    if start / 8 > 0x1fff {
        return Err(SystemError::EMSGSIZE);
    }
    let mut fragment = Vec::new();
    fragment
        .try_reserve_exact(parsed.header_len + chunk)
        .map_err(|_| SystemError::ENOMEM)?;
    fragment.extend_from_slice(&packet.frame[..parsed.header_len]);
    fragment.extend_from_slice(
        &packet.frame[parsed.header_len + offset..parsed.header_len + offset + chunk],
    );
    if offset != 0 {
        let mut option = 20;
        while option < parsed.header_len {
            let kind = fragment[option];
            if kind == 0 {
                break;
            }
            if kind == 1 {
                option += 1;
                continue;
            }
            let len = fragment[option + 1] as usize;
            if kind & 0x80 == 0 {
                fragment[option..option + len].fill(1);
            }
            option += len;
        }
    }
    fragment[2..4].copy_from_slice(&((parsed.header_len + chunk) as u16).to_be_bytes());
    let mut flags = (start / 8) as u16;
    if remaining > chunk || parsed.more_fragments {
        flags |= 0x2000;
    }
    fragment[6..8].copy_from_slice(&flags.to_be_bytes());
    fragment[10..12].fill(0);
    smoltcp::wire::Ipv4Packet::new_unchecked(&mut fragment[..]).fill_checksum();
    Ok((fragment, chunk))
}

fn next_output_fragment(
    packet: &LocalOutputPacket,
    mtu: usize,
) -> Result<Option<(Vec<u8>, usize)>, SystemError> {
    if packet.frame.first().is_some_and(|byte| byte >> 4 == 6) && mtu < 1280 {
        return Err(SystemError::ENETDOWN);
    }
    match packet.prepared_ip {
        Some(PreparedIpProgress::Ipv4 {
            offset,
            may_fragment,
            max_fragment_len,
        }) if offset != 0 || packet.frame.len() > mtu.min(max_fragment_len.unwrap_or(mtu)) => {
            if max_fragment_len.is_some_and(|max| max > mtu)
                && ParsedIpv4Output::parse(&packet.frame)?.df
            {
                return Err(SystemError::EMSGSIZE);
            }
            if !may_fragment {
                return Err(SystemError::EMSGSIZE);
            }
            prepared_ipv4_fragment(packet, mtu.min(max_fragment_len.unwrap_or(mtu))).map(Some)
        }
        Some(PreparedIpProgress::Ipv6 {
            offset,
            max_fragment_len,
            ..
        }) if offset != 0
            || packet.frame.len() > mtu.min(max_fragment_len.unwrap_or(mtu).max(1280)) =>
        {
            if max_fragment_len.is_some_and(|max| max > mtu) {
                return Err(SystemError::EMSGSIZE);
            }
            prepared_ipv6_fragment(packet, mtu.min(max_fragment_len.unwrap_or(mtu).max(1280)))
                .map(Some)
        }
        None if packet.frame.len() > mtu => Err(SystemError::EMSGSIZE),
        _ => Ok(None),
    }
}

/// Source-fragment a plain IPv6 datagram only after its complete packet has
/// passed OUTPUT and POST_ROUTING. The original transport checksum is retained.
fn prepared_ipv6_fragment(
    packet: &LocalOutputPacket,
    mtu: usize,
) -> Result<(Vec<u8>, usize), SystemError> {
    let Some(PreparedIpProgress::Ipv6 {
        offset,
        identification,
        ..
    }) = packet.prepared_ip
    else {
        return Err(SystemError::EINVAL);
    };
    let ipv6 = smoltcp::wire::Ipv6Packet::new_checked(packet.frame.as_slice())
        .map_err(|_| SystemError::EINVAL)?;
    if ipv6.total_len() != packet.frame.len() {
        return Err(SystemError::EMSGSIZE);
    }
    let (split, previous_next_header) = ipv6_fragment_boundary(&packet.frame)?;
    if mtu < split + 16 {
        return Err(SystemError::EMSGSIZE);
    }
    let payload_len = packet.frame.len() - split;
    if offset >= payload_len {
        return Err(SystemError::EINVAL);
    }
    let remaining = payload_len - offset;
    let available = mtu - split - 8;
    let chunk = if remaining > available {
        available & !7
    } else {
        remaining
    };
    if chunk == 0 {
        return Err(SystemError::EMSGSIZE);
    }
    let mut fragment = Vec::new();
    fragment
        .try_reserve_exact(split + 8 + chunk)
        .map_err(|_| SystemError::ENOMEM)?;
    fragment.extend_from_slice(&packet.frame[..split]);
    fragment.extend_from_slice(&[0; 8]);
    fragment.extend_from_slice(&packet.frame[split + offset..split + offset + chunk]);
    fragment[4..6].copy_from_slice(&((split - 40 + 8 + chunk) as u16).to_be_bytes());
    fragment[previous_next_header] = smoltcp::wire::IpProtocol::Ipv6Frag.into();
    fragment[split] = packet.frame[previous_next_header];
    let more = u16::from(remaining > chunk);
    let offset_flags = ((offset / 8) as u16) << 3 | more;
    fragment[split + 2..split + 4].copy_from_slice(&offset_flags.to_be_bytes());
    fragment[split + 4..split + 8].copy_from_slice(&identification.to_be_bytes());
    Ok((fragment, chunk))
}

/// The unfragmentable chain is the fixed header, HBH, Routing, and any
/// Destination Options before Routing. Linux 6.6 `ip6_find_1stfragopt()`
/// stops at the first post-Routing Destination Options or other header.
fn ipv6_fragment_boundary(bytes: &[u8]) -> Result<(usize, usize), SystemError> {
    if bytes.len() < 40 || bytes[0] >> 4 != 6 {
        return Err(SystemError::EINVAL);
    }
    let mut next = bytes[6];
    let mut previous_next_header = 6;
    let mut offset = 40;
    let mut found_routing = false;
    loop {
        match next {
            0 | 43 => {
                found_routing |= next == 43;
            }
            60 if !found_routing => {}
            _ => break,
        }
        if offset + 2 > bytes.len() {
            return Err(SystemError::EINVAL);
        }
        let length = (usize::from(bytes[offset + 1]) + 1) * 8;
        if length < 8 || offset + length > bytes.len() {
            return Err(SystemError::EINVAL);
        }
        previous_next_header = offset;
        next = bytes[offset];
        offset += length;
    }
    if offset >= bytes.len() || next == 44 {
        return Err(SystemError::EMSGSIZE);
    }
    Ok((offset, previous_next_header))
}

fn fragment_transmit_error(
    packet: LocalOutputPacket,
    error: SystemError,
) -> LocalOutputTransmitResult {
    if error == SystemError::ENOMEM {
        // A complete datagram was already admitted. Retry with the existing
        // bounded TX-backoff rather than losing it to transient allocation
        // pressure or spinning in the current poll round.
        LocalOutputTransmitResult::RetrySoon(packet)
    } else {
        LocalOutputTransmitResult::Drop(packet, error)
    }
}

fn fragment_transmit_complete(
    packet: &mut LocalOutputPacket,
    fragment: &Option<(Vec<u8>, usize)>,
) -> bool {
    let Some((_, payload_len)) = fragment else {
        return true;
    };
    match packet.prepared_ip.as_mut() {
        Some(PreparedIpProgress::Ipv4 { offset, .. }) => {
            *offset += payload_len;
            let header_len = ((packet.frame[0] & 0x0f) as usize) * 4;
            *offset == packet.frame.len() - header_len
        }
        Some(PreparedIpProgress::Ipv6 { offset, .. }) => {
            *offset += payload_len;
            ipv6_fragment_boundary(&packet.frame)
                .is_ok_and(|(split, _)| *offset == packet.frame.len() - split)
        }
        None => false,
    }
}

#[cfg(test)]
mod prepared_ipv4_tests {
    use super::*;

    fn datagram(options: &[u8], payload_len: usize) -> LocalOutputPacket {
        let header_len = 20 + options.len();
        let mut frame = alloc::vec![0; header_len + payload_len];
        frame[0] = 0x40 | (header_len / 4) as u8;
        let total_len = frame.len() as u16;
        frame[2..4].copy_from_slice(&total_len.to_be_bytes());
        frame[4..6].copy_from_slice(&0x55aau16.to_be_bytes());
        frame[8] = 64;
        frame[9] = 17;
        frame[12..16].copy_from_slice(&[192, 0, 2, 1]);
        frame[16..20].copy_from_slice(&[192, 0, 2, 2]);
        frame[20..header_len].copy_from_slice(options);
        for (index, byte) in frame[header_len..].iter_mut().enumerate() {
            *byte = index as u8;
        }
        smoltcp::wire::Ipv4Packet::new_unchecked(&mut frame[..]).fill_checksum();
        LocalOutputPacket {
            medium: smoltcp::phy::Medium::Ip,
            meta: PacketMeta::default(),
            disposition: LocalOutputDisposition::Routed {
                oif: 2,
                next_hop: smoltcp::wire::Ipv4Address::new(192, 0, 2, 2).into(),
                ip_mtu: 1500,
            },
            frame,
            ct_context: OutputCtContext::Untracked,
            mark: 0,
            prepared_ip: Some(PreparedIpProgress::Ipv4 {
                offset: 0,
                may_fragment: true,
                max_fragment_len: None,
            }),
            forward_mtu_feedback: None,
            _charge: None,
        }
    }

    #[test]
    fn deferred_generated_ipv4_uses_unique_fragment_id_only_when_needed() {
        let mut small = datagram(&[], 24).frame;
        small[6] |= 0x40;
        small[4..6].fill(0);
        assert_eq!(
            Ok(false),
            prepare_deferred_ipv4_for_mtu(&mut small, 1500, Some(7))
        );
        assert_eq!(&small[4..6], &[0, 0]);
        assert_ne!(small[6] & 0x40, 0);

        let mut large = small.clone();
        assert_eq!(
            Ok(true),
            prepare_deferred_ipv4_for_mtu(&mut large, 28, Some(0x1234))
        );
        assert_eq!(&large[4..6], &0x1234u16.to_be_bytes());
        assert_eq!(large[6] & 0x40, 0);
        assert!(smoltcp::wire::Ipv4Packet::new_checked(&large[..])
            .unwrap()
            .verify_checksum());
        assert_eq!(
            Err(smoltcp::phy::IpOutputError::MtuExceeded),
            prepare_deferred_ipv4_for_mtu(&mut small, 28, None)
        );
    }

    #[test]
    fn successive_fragments_keep_id_offset_payload_and_checksum() {
        let mut packet = datagram(&[], 24);
        for (offset, expected_mf) in [(0, true), (8, true), (16, false)] {
            let (fragment, payload_len) = prepared_ipv4_fragment(&packet, 28).unwrap();
            let header = smoltcp::wire::Ipv4Packet::new_checked(&fragment[..]).unwrap();
            assert!(header.verify_checksum());
            assert_eq!(header.ident(), 0x55aa);
            assert_eq!(
                u16::from_be_bytes([fragment[6], fragment[7]]) & 0x1fff,
                (offset / 8) as u16
            );
            assert_eq!(fragment[6] & 0x20 != 0, expected_mf);
            assert_eq!(&fragment[20..], &packet.frame[20 + offset..20 + offset + 8]);
            assert_eq!(payload_len, 8);
            // A rejected physical token must leave this cursor unchanged.
            assert!(
                matches!(packet.prepared_ip, Some(PreparedIpProgress::Ipv4 { offset: current, .. }) if current == offset)
            );
            assert_eq!(
                fragment_transmit_complete(&mut packet, &Some((fragment, payload_len))),
                offset == 16
            );
        }
    }

    #[test]
    fn only_copied_ipv4_options_survive_later_fragments() {
        let mut packet = datagram(&[0x94, 4, 0, 0, 0x44, 4, 5, 0], 24);
        let (first, sent) = prepared_ipv4_fragment(&packet, 44).unwrap();
        assert_eq!(&first[20..28], &[0x94, 4, 0, 0, 0x44, 4, 5, 0]);
        assert!(!fragment_transmit_complete(
            &mut packet,
            &Some((first, sent))
        ));
        let (second, _) = prepared_ipv4_fragment(&packet, 44).unwrap();
        assert_eq!(&second[20..28], &[0x94, 4, 0, 0, 1, 1, 1, 1]);
        assert!(smoltcp::wire::Ipv4Packet::new_checked(&second[..])
            .unwrap()
            .verify_checksum());
    }

    #[test]
    fn invalid_option_length_is_rejected_before_output() {
        let mut packet = datagram(&[0x83, 1, 0, 0], 8);
        assert!(matches!(
            ParsedIpv4Output::parse(&packet.frame),
            Err(SystemError::EINVAL)
        ));
        packet.frame[21] = 4;
        assert!(ParsedIpv4Output::parse(&packet.frame).is_ok());
    }

    #[test]
    fn fragment_allocation_failure_keeps_original_for_backoff() {
        let packet = datagram(&[], 24);
        let result = fragment_transmit_error(packet, SystemError::ENOMEM);
        let LocalOutputTransmitResult::RetrySoon(packet) = result else {
            panic!("an admitted packet must remain queued after ENOMEM");
        };
        assert!(matches!(
            packet.prepared_ip,
            Some(PreparedIpProgress::Ipv4 { offset: 0, .. })
        ));
    }

    #[test]
    fn reassembled_ipv4_keeps_original_fragment_limit_and_df_boundary() {
        let mut packet = datagram(&[], 96);
        packet.prepared_ip = Some(PreparedIpProgress::Ipv4 {
            offset: 0,
            may_fragment: true,
            max_fragment_len: Some(60),
        });
        let mut lengths = Vec::new();
        loop {
            let (fragment, chunk) = next_output_fragment(&packet, 1500).unwrap().unwrap();
            lengths.push(fragment.len());
            if fragment_transmit_complete(&mut packet, &Some((fragment, chunk))) {
                break;
            }
        }
        assert_eq!(lengths, [60, 60, 36]);

        let mut df = datagram(&[], 96);
        df.frame[6] |= 0x40;
        df.prepared_ip = Some(PreparedIpProgress::Ipv4 {
            offset: 0,
            may_fragment: true,
            max_fragment_len: Some(60),
        });
        assert!(matches!(
            next_output_fragment(&df, 52),
            Err(SystemError::EMSGSIZE)
        ));
    }
}

#[cfg(test)]
mod prepared_ipv6_tests {
    use super::*;

    fn datagram(payload_len: usize) -> LocalOutputPacket {
        let udp_len = payload_len + 8;
        let mut frame = alloc::vec![0; 40 + udp_len];
        frame[0] = 0x60;
        frame[4..6].copy_from_slice(&(udp_len as u16).to_be_bytes());
        frame[6] = smoltcp::wire::IpProtocol::Udp.into();
        frame[7] = 64;
        frame[23] = 1;
        frame[39] = 1;
        frame[40..42].copy_from_slice(&1234u16.to_be_bytes());
        frame[42..44].copy_from_slice(&4321u16.to_be_bytes());
        frame[44..46].copy_from_slice(&(udp_len as u16).to_be_bytes());
        frame[46..48].copy_from_slice(&0x55aau16.to_be_bytes());
        for (index, byte) in frame[48..].iter_mut().enumerate() {
            *byte = index as u8;
        }
        LocalOutputPacket {
            medium: smoltcp::phy::Medium::Ip,
            meta: PacketMeta::default(),
            disposition: LocalOutputDisposition::Routed {
                oif: 2,
                next_hop: smoltcp::wire::Ipv6Address::LOCALHOST.into(),
                ip_mtu: 1280,
            },
            frame,
            ct_context: OutputCtContext::Untracked,
            mark: 0,
            prepared_ip: Some(PreparedIpProgress::Ipv6 {
                offset: 0,
                identification: 0x1234_5678,
                max_fragment_len: None,
            }),
            forward_mtu_feedback: None,
            _charge: None,
        }
    }

    #[test]
    fn fragments_preserve_one_id_checksum_and_contiguous_payload_across_mtu_change() {
        let mut packet = datagram(40);
        let mut reassembled = Vec::new();
        for (mtu, expected_offset, expected_more) in
            [(64, 0, true), (72, 16, true), (72, 40, false)]
        {
            let first = next_output_fragment(&packet, mtu).unwrap().unwrap();
            let retry = next_output_fragment(&packet, mtu).unwrap().unwrap();
            assert_eq!(first, retry, "a rejected token must not advance the cursor");
            let (fragment, chunk) = first;
            let ipv6 = smoltcp::wire::Ipv6Packet::new_checked(fragment.as_slice()).unwrap();
            assert_eq!(ipv6.next_header(), smoltcp::wire::IpProtocol::Ipv6Frag);
            assert_eq!(fragment[40], smoltcp::wire::IpProtocol::Udp.into());
            assert_eq!(&fragment[44..48], &0x1234_5678u32.to_be_bytes());
            let flags = u16::from_be_bytes([fragment[42], fragment[43]]);
            assert_eq!((flags >> 3) as usize * 8, expected_offset);
            assert_eq!(flags & 1 != 0, expected_more);
            reassembled.extend_from_slice(&fragment[48..]);
            assert_eq!(
                fragment_transmit_complete(&mut packet, &Some((fragment, chunk))),
                !expected_more
            );
        }
        assert_eq!(reassembled, packet.frame[40..]);
        assert_eq!(&reassembled[6..8], &0x55aau16.to_be_bytes());
    }

    #[test]
    fn too_small_mtu_never_advances_or_emits_a_fragment() {
        let packet = datagram(40);
        assert!(matches!(
            next_output_fragment(&packet, 55),
            Err(SystemError::EMSGSIZE)
        ));
        assert!(matches!(
            packet.prepared_ip,
            Some(PreparedIpProgress::Ipv6 { offset: 0, .. })
        ));
    }

    #[test]
    fn raw_ipv6_fragment_preserves_original_next_header() {
        let mut packet = datagram(40);
        packet.frame[6] = smoltcp::wire::IpProtocol::Icmpv6.into();
        let (fragment, chunk) = prepared_ipv6_fragment(&packet, 64).unwrap();
        assert_eq!(chunk, 16);
        assert_eq!(fragment[6], smoltcp::wire::IpProtocol::Ipv6Frag.into());
        assert_eq!(fragment[40], smoltcp::wire::IpProtocol::Icmpv6.into());
        assert_eq!(&fragment[48..], &packet.frame[40..56]);
        assert!(matches!(
            packet.prepared_ip,
            Some(PreparedIpProgress::Ipv6 { offset: 0, .. })
        ));
    }

    #[test]
    fn reassembled_ipv6_keeps_original_limit_and_extension_boundary() {
        let mut packet = datagram(2000);
        // Hop-by-Hop is unfragmentable; the Fragment header follows it.
        packet.frame.splice(40..40, [17, 0, 0, 0, 0, 0, 0, 0]);
        packet.frame[6] = 0;
        let payload_len = (packet.frame.len() - 40) as u16;
        packet.frame[4..6].copy_from_slice(&payload_len.to_be_bytes());
        packet.prepared_ip = Some(PreparedIpProgress::Ipv6 {
            offset: 0,
            identification: 42,
            max_fragment_len: Some(1300),
        });
        let mut fragments = 0;
        loop {
            let (fragment, chunk) = next_output_fragment(&packet, 1500).unwrap().unwrap();
            assert!(fragment.len() <= 1300);
            assert_eq!(fragment[6], 0);
            assert_eq!(fragment[40], 44);
            fragments += 1;
            if fragment_transmit_complete(&mut packet, &Some((fragment, chunk))) {
                break;
            }
        }
        assert_eq!(fragments, 2);
        assert!(matches!(
            next_output_fragment(&packet, 1200),
            Err(SystemError::EMSGSIZE)
        ));
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
    fn deferred_ip_output(&self, version: smoltcp::wire::IpVersion) -> bool {
        self.backend_policy.requires_output_admission(version)
    }

    fn consume_full_ip<F>(
        mut self,
        len: usize,
        meta: PacketMeta,
        class: smoltcp::phy::IpOutputClass,
        ipv4_fragment_ident: Option<u16>,
        emit: F,
    ) -> Result<(), smoltcp::phy::IpOutputError>
    where
        F: FnOnce(&mut [u8]),
    {
        use smoltcp::phy::IpOutputError;
        use smoltcp::wire::{IpAddress, IpVersion, Ipv4Packet, Ipv6Address, Ipv6Packet};

        if !self.scratch.try_ensure_capacity(len, &mut self.reservation) {
            return Err(IpOutputError::Exhausted);
        }
        let bytes = self.scratch.resize(len);
        emit(bytes);
        let destination = match IpVersion::of_packet(bytes) {
            Ok(IpVersion::Ipv4) => Ipv4Packet::new_checked(&bytes[..])
                .map(|packet| IpAddress::Ipv4(packet.dst_addr()))
                .map_err(|_| IpOutputError::NoRoute)?,
            Ok(IpVersion::Ipv6) => Ipv6Packet::new_checked(&bytes[..])
                .map(|packet| IpAddress::Ipv6(packet.dst_addr()))
                .map_err(|_| IpOutputError::NoRoute)?,
            Err(_) => return Err(IpOutputError::NoRoute),
        };
        let mut route = if class == smoltcp::phy::IpOutputClass::LinkLocalControl
            || destination.version() == IpVersion::Ipv6
                && destination.is_multicast()
                && self.backend_policy.owner_ifindex != 0
        {
            if self.backend_policy.owner_ifindex == 0 || !self.backend_policy.owner_is_up {
                return Err(IpOutputError::NoRoute);
            }
            let ip_mtu = self
                .backend_policy
                .routes
                .device_mtu(self.backend_policy.owner_ifindex)
                .ok_or(IpOutputError::NoRoute)?;
            crate::net::route::OutputRouteDecision {
                oif: self.backend_policy.owner_ifindex,
                required_oif: Some(self.backend_policy.owner_ifindex),
                next_hop: destination,
                ip_mtu,
                kind: crate::net::route::RTN_MULTICAST,
                table: crate::net::route::RT_TABLE_LOCAL,
            }
        } else {
            let required_oif = (meta.id != 0).then_some(meta.id);
            self.backend_policy
                .routes
                .lookup(destination, required_oif)
                .ok_or(IpOutputError::NoRoute)?
        };
        let version = destination.version();
        let ipv4 = version == IpVersion::Ipv4;
        let ruleset = self
            .backend_policy
            .ruleset
            .ok_or(IpOutputError::PolicyDrop)?;
        let ct =
            crate::net::output::LocalOutputCt::new(self.backend_policy.netns, ruleset, version);
        let output_hook_oif = if route.kind == crate::net::route::RTN_LOCAL {
            crate::net::LOOPBACK_IFINDEX as u32
        } else {
            route.oif
        };
        let output_name = self
            .backend_policy
            .hook_oifname(
                crate::net::nftables::NftIpv4Hook::LocalOut,
                version,
                output_hook_oif,
            )
            .ok_or(IpOutputError::PolicyDrop)?;
        let lookup = |address| self.backend_policy.routes.ipv4_addr_type(address);
        let routes = self.backend_policy.routes;
        let ipv6_is_local = |address: Ipv6Address| {
            routes
                .lookup(IpAddress::Ipv6(address), None)
                .is_some_and(|decision| decision.kind == crate::net::route::RTN_LOCAL)
        };
        let output_allowed = if ipv4 {
            ct.evaluate_ipv4(
                crate::net::nftables::NftIpv4Hook::LocalOut,
                bytes,
                output_name,
                &lookup,
                None,
            )
        } else {
            ct.evaluate_ipv6(
                crate::net::nftables::NftIpv4Hook::LocalOut,
                bytes,
                output_name,
                Some(&ipv6_is_local),
                None,
            )
        }
        .map_err(|_| IpOutputError::PolicyDrop)?;
        if !output_allowed {
            return Err(IpOutputError::PolicyDrop);
        }
        let new_destination = match version {
            IpVersion::Ipv4 => Ipv4Packet::new_checked(&bytes[..])
                .map(|packet| IpAddress::Ipv4(packet.dst_addr()))
                .map_err(|_| IpOutputError::PolicyDrop)?,
            IpVersion::Ipv6 => Ipv6Packet::new_checked(&bytes[..])
                .map(|packet| IpAddress::Ipv6(packet.dst_addr()))
                .map_err(|_| IpOutputError::PolicyDrop)?,
        };
        if new_destination != destination {
            route = self
                .backend_policy
                .routes
                .lookup(new_destination, route.required_oif)
                .ok_or(IpOutputError::NoRoute)?;
            let egress = self
                .backend_policy
                .routes
                .ingress_device(route.oif)
                .ok_or(IpOutputError::NoRoute)?;
            if route.kind != crate::net::route::RTN_LOCAL
                && !egress.flags().contains(InterfaceFlags::UP)
            {
                return Err(IpOutputError::NoRoute);
            }
        }
        let disposition = if route.kind == crate::net::route::RTN_LOCAL {
            LocalOutputDisposition::Local {
                oif: route.oif,
                ip_mtu: route.ip_mtu,
            }
        } else if matches!(
            route.kind,
            crate::net::route::RTN_UNICAST
                | crate::net::route::RTN_BROADCAST
                | crate::net::route::RTN_MULTICAST
        ) {
            LocalOutputDisposition::Routed {
                oif: route.oif,
                next_hop: route.next_hop,
                ip_mtu: route.ip_mtu,
            }
        } else {
            return Err(IpOutputError::NoRoute);
        };
        let ip_mtu = route.ip_mtu;
        let hook_oif = if route.kind == crate::net::route::RTN_LOCAL {
            crate::net::LOOPBACK_IFINDEX as u32
        } else {
            route.oif
        };
        let may_fragment = if ipv4 {
            prepare_deferred_ipv4_for_mtu(bytes, ip_mtu, ipv4_fragment_ident)?
        } else {
            if len > ip_mtu {
                return Err(IpOutputError::MtuExceeded);
            }
            false
        };
        let post_name = self
            .backend_policy
            .hook_oifname(
                crate::net::nftables::NftIpv4Hook::PostRouting,
                version,
                hook_oif,
            )
            .ok_or(IpOutputError::PolicyDrop)?;
        let masquerade = if ruleset
            .requires_masquerade(version, crate::net::nftables::NftIpv4Hook::PostRouting)
        {
            self.backend_policy
                .routes
                .masquerade_address(hook_oif, new_destination, route.next_hop)
                .map(|address| {
                    let address = match address {
                        IpAddress::Ipv4(address) => {
                            crate::net::conntrack::CtAddress::V4(address.octets())
                        }
                        IpAddress::Ipv6(address) => {
                            crate::net::conntrack::CtAddress::V6(address.octets())
                        }
                    };
                    (address, hook_oif)
                })
        } else {
            None
        };
        let post_allowed = if ipv4 {
            ct.evaluate_ipv4(
                crate::net::nftables::NftIpv4Hook::PostRouting,
                bytes,
                post_name,
                &lookup,
                masquerade,
            )
        } else {
            ct.evaluate_ipv6(
                crate::net::nftables::NftIpv4Hook::PostRouting,
                bytes,
                post_name,
                Some(&ipv6_is_local),
                masquerade,
            )
        }
        .map_err(|_| IpOutputError::PolicyDrop)?;
        if !post_allowed {
            return Err(IpOutputError::PolicyDrop);
        }
        let mark = ct.mark();
        let ct_context = ct.confirm().map_err(|_| IpOutputError::PolicyDrop)?;
        if ipv4 {
            let frame = self.scratch.take().ok_or(IpOutputError::Exhausted)?;
            self.reservation.commit_prepared_ipv4(
                frame,
                meta,
                disposition,
                may_fragment,
                None,
                ct_context,
                mark,
            );
        } else {
            let frame = self.scratch.take().ok_or(IpOutputError::Exhausted)?;
            self.reservation.commit_prepared_ipv6(
                frame,
                meta,
                disposition,
                self.backend_policy
                    .netns
                    .next_ipv6_fragment_identification(),
                None,
                ct_context,
                mark,
            );
        }
        Ok(())
    }

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
            let next_hop = route.next_hop;
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
    fn deferred_ip_output(&self, version: smoltcp::wire::IpVersion) -> bool {
        self.backend_policy.requires_output_admission(version)
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
        let token = local_tx_token(self.queue, self.backend_policy, self.capabilities)
            .ok_or(smoltcp::phy::IpOutputError::Exhausted)?;
        token.consume_full_ip(len, meta, class, ipv4_fragment_ident, emit)
    }

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
    #[expect(
        clippy::too_many_arguments,
        reason = "poll-scoped packet cells must remain separately borrowed"
    )]
    pub(super) fn new(
        device: &'a mut D,
        common: &'a IfaceCommon,
        backend_policy: OutputBackendPolicy<'a>,
        stage_cell: Option<&'a Cell<IngressStage>>,
        broadcast_cell: Option<&'a Cell<bool>>,
        ct_context_cell: Option<
            &'a core::cell::RefCell<Option<crate::net::conntrack::CtPacketContext>>,
        >,
        mark_cell: Option<&'a Cell<u32>>,
        forward_fragment_cell: Option<
            &'a Cell<Option<crate::net::forward_mtu::ReassembledForwardInfo>>,
        >,
    ) -> Self {
        Self {
            device,
            common,
            backend_policy,
            stage_cell,
            broadcast_cell,
            ct_context_cell,
            mark_cell,
            forward_fragment_cell,
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
    // The device capability is the lifetime maximum accepted by smoltcp;
    // a bridge/veth starts at 1500 and can later grow to jumbo MTU. Charge
    // the current owner MTU, not that maximum, for every ordinary packet.
    let owner_ip_mtu = backend_policy
        .routes
        .ingress_device(backend_policy.owner_ifindex)
        .map_or(capabilities.ip_mtu(), |iface| iface.mtu());
    let frame_capacity = owner_ip_mtu
        + if capabilities.medium == smoltcp::phy::Medium::Ethernet {
            14
        } else {
            0
        };
    let scratch = LocalInputScratch::checkout(&queue.response_scratch, frame_capacity)?;
    if !reservation.try_resize(scratch.capacity()) {
        return None;
    }
    Some(LocalInputTxToken {
        medium: capabilities.medium,
        meta: PacketMeta::default(),
        disposition: LocalOutputDisposition::NativeOwner,
        backend_policy,
        owner_ip_mtu,
        reservation,
        scratch,
    })
}

impl<D: SmolDevice + ?Sized> SmolDevice for LocalInputDevice<'_, D> {
    fn policy_current(&self) -> bool {
        self.backend_policy.policy_current()
    }

    fn outbound_ip_mtu(&self, destination: smoltcp::wire::IpAddress, meta: PacketMeta) -> usize {
        self.backend_policy
            .outbound_ip_mtu(destination, meta, self.device.capabilities().ip_mtu())
    }

    type RxToken<'a>
        = LocalInputRxToken<'a>
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
        let mut packet = self.common.local_input_queue.pop()?;
        let ingress_ifindex = packet.ingress_ifindex;
        let ingress_stage = packet.ingress_stage;
        let broadcast = packet.broadcast;
        let ct_context = packet.ct_context.take();
        let mark = packet.mark;
        let forward_fragment = packet.forward_fragment;
        let frame = packet.into_frame(self.device.capabilities().medium).ok()?;
        let mut meta = PacketMeta::default();
        meta.id = ingress_ifindex;
        Some((
            LocalInputRxToken {
                frame,
                meta,
                ingress_stage,
                stage_cell: self.stage_cell,
                broadcast,
                broadcast_cell: self.broadcast_cell,
                ct_context,
                ct_context_cell: self.ct_context_cell,
                mark,
                mark_cell: self.mark_cell,
                forward_fragment,
                forward_fragment_cell: self.forward_fragment_cell,
            },
            tx_token,
        ))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        self.tx_token()
    }

    fn capabilities(&self) -> DeviceCapabilities {
        self.device.capabilities()
    }
}

impl<D: SmolDevice + ?Sized> SmolDevice for RoutedTxDevice<'_, D> {
    fn policy_current(&self) -> bool {
        self.backend_policy.policy_current()
    }

    fn outbound_ip_mtu(&self, destination: smoltcp::wire::IpAddress, meta: PacketMeta) -> usize {
        self.backend_policy
            .outbound_ip_mtu(destination, meta, self.device.capabilities().ip_mtu())
    }

    type RxToken<'a>
        = RoutedRxToken<D::RxToken<'a>>
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
            RoutedRxToken {
                inner: rx_token,
                ifindex: self.backend_policy.owner_ifindex,
            },
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
    Continue(LocalOutputPacket),
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
    mut packet: LocalOutputPacket,
) -> LocalOutputTransmitResult {
    let LocalOutputDisposition::Routed {
        next_hop, ip_mtu, ..
    } = packet.disposition
    else {
        return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
    };
    if packet.medium != smoltcp::phy::Medium::Ip
        || (packet.prepared_ip.is_none() && packet.frame.len() > ip_mtu)
        || smoltcp::wire::IpVersion::of_packet(&packet.frame).ok() != Some(next_hop.version())
    {
        return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
    }
    if !iface.flags().contains(InterfaceFlags::UP) {
        return LocalOutputTransmitResult::Drop(packet, SystemError::ENETDOWN);
    }
    let effective_mtu = ip_mtu.min(iface.mtu());
    let fragment = match next_output_fragment(&packet, effective_mtu) {
        Ok(fragment) => fragment,
        Err(error) => return fragment_transmit_error(packet, error),
    };
    let wire_packet = fragment
        .as_ref()
        .map_or(packet.frame.as_slice(), |(frame, _)| frame.as_slice());
    // Loopback's route_and_send creates a fresh local-input token without a
    // sidecar. A tracked OUTPUT packet must carry its confirmed identity into
    // PRE_ROUTING, including each multicast fragment. Preserve the same lo
    // queue/backlog behavior while passing that packet-owned context.
    let transmitted = if iface.flags().contains(InterfaceFlags::LOOPBACK) {
        crate::driver::net::inject_local_ip_packet_with_context_and_mark(
            iface,
            crate::net::LOOPBACK_IFINDEX as u32,
            iface.mac(),
            wire_packet,
            false,
            LocalPacketOrigin::LocalOutput,
            Some(packet.ct_context.for_ingress()),
            packet.mark,
        )
        .map_err(RouteSendError::Failed)
    } else {
        iface.route_and_send(&next_hop, wire_packet)
    };
    match transmitted {
        Ok(()) => {
            if !fragment_transmit_complete(&mut packet, &fragment) {
                return LocalOutputTransmitResult::Continue(packet);
            }
            LocalOutputTransmitResult::Sent(packet)
        }
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
        LocalOutputTransmitResult::Continue(packet) => {
            reservation.requeue_ready(packet);
            AdmittedRoutedOutput::Queued(crate::time::Instant::now().into())
        }
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
    mut packet: LocalOutputPacket,
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
                || (packet.prepared_ip.is_none() && packet.frame.len() > ip_mtu)
                || smoltcp::wire::IpVersion::of_packet(&packet.frame).is_err()
            {
                return LocalOutputTransmitResult::Drop(packet, SystemError::EINVAL);
            }
            let Some(iface) = netns.device_list().get(&(oif as usize)).cloned() else {
                return LocalOutputTransmitResult::Drop(packet, SystemError::ENODEV);
            };
            let effective_mtu = ip_mtu.min(iface.mtu());
            let fragment = match next_output_fragment(&packet, effective_mtu) {
                Ok(fragment) => fragment,
                Err(error) => return fragment_transmit_error(packet, error),
            };
            // IPv6 local delivery uses the address-owning device as its
            // logical input interface even though the packet traverses lo.
            // Linux ip6_rcv_core stores the route's device in IP6CB(skb)->iif;
            // SO_BINDTODEVICE must observe that scoped interface.
            let ingress_ifindex = if matches!(
                smoltcp::wire::IpVersion::of_packet(&packet.frame),
                Ok(smoltcp::wire::IpVersion::Ipv6)
            ) {
                oif
            } else {
                crate::net::LOOPBACK_IFINDEX as u32
            };
            let wire_packet = fragment
                .as_ref()
                .map_or(packet.frame.as_slice(), |(frame, _)| frame.as_slice());
            match crate::driver::net::inject_local_ip_packet_with_context_and_mark(
                iface.as_ref(),
                ingress_ifindex,
                iface.mac(),
                wire_packet,
                false,
                LocalPacketOrigin::LocalOutput,
                Some(packet.ct_context.for_ingress()),
                packet.mark,
            ) {
                Ok(()) => {
                    if !fragment_transmit_complete(&mut packet, &fragment) {
                        return LocalOutputTransmitResult::Continue(packet);
                    }
                    LocalOutputTransmitResult::Sent(packet)
                }
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
