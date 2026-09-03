use super::*;

pub struct IfaceCommon {
    pub(super) iface_id: usize,
    pub(super) name: RwLock<String>,
    pub(super) flags: AtomicU32,
    pub(super) mtu: AtomicUsize,
    pub(super) type_: InterfaceType,
    pub(super) smol_iface: Mutex<smoltcp::iface::Interface>,
    /// 存smoltcp网卡的套接字集
    pub(super) sockets: Mutex<smoltcp::iface::SocketSet<'static>>,
    /// 存 kernel wrap smoltcp socket 的集合
    pub(super) bounds: RwLock<Arc<Vec<Arc<dyn InetSocket>>>>,
    /// Lock-free lifecycle summary for DOWN-owner protocol progress. The
    /// vector remains authoritative; this count only decides poll eligibility.
    pub(super) bound_socket_count: AtomicUsize,
    /// Sockets that can already emit through smoltcp but have not yet been
    /// published in `bounds`.
    pending_routed_socket_count: AtomicUsize,
    /// The stack has accepted namespace-local ingress and must keep applying
    /// authoritative output routing until its socket/deferred work quiesces.
    pub(super) namespace_routed_stack: AtomicBool,
    /// 端口管理器
    pub(super) port_manager: PortManager,
    /// Scheduler-owned future protocol deadline. Immediate work stays with
    /// the current poll owner and is never armed here.
    pub(super) poll_deadline: PollDeadline,
    /// Bounded fallback delay for local-output device backpressure. Individual
    /// packets retain their not-before deadline across unrelated poll wakes.
    pub(super) local_output_tx_backoff_us: AtomicU64,
    /// Monotonic handshake between TX completion and backpressure enqueue.
    /// A producer that observes a change after enqueue promotes the packet
    /// itself; otherwise the later completion notification does so.
    pub(super) tx_completion_generation: AtomicU64,
    /// 网络命名空间
    pub(super) net_namespace: RwLock<Weak<NetNamespace>>,
    /// 路由相关数据
    pub(super) router_common_data: RouterEnableDeviceCommon,
    /// NAPI 结构体
    pub(super) napi_struct: RwLock<Option<Arc<NapiStruct>>>,
    /// Namespace-local frames handed to this interface's protocol stack.
    /// This is shared by every interface implementation so weak-host local
    /// delivery never depends on a driver-specific receive queue.
    pub(super) local_input_queue: LocalInputQueue,
    /// Per-address control-plane state committed together with smoltcp's
    /// address list. It carries Linux IFA_LABEL semantics and opaque ownership
    /// tokens for in-kernel actors such as DHCP.
    pub(super) address_metadata: Mutex<Vec<AddressMetadata>>,
    /// Routes supplied by constructors before the interface joins a netns.
    /// Drained transactionally by netns registration; never authoritative.
    pub(super) bootstrap_routes: Mutex<Vec<BootstrapRoute>>,
    pub(super) static_neighbors: RwSem<Vec<StaticNeighborEntry>>,
    /// TCP close(2) 语义辅助：延迟回收 smoltcp TCP socket（Linux-like）。
    pub(super) tcp_close_defer: crate::net::tcp_close_defer::TcpCloseDefer,
    /// TCP listener/backlog 语义辅助（Linux-like 丢 SYN 等）。
    pub(super) tcp_listener_backlog: crate::net::tcp_listener_backlog::TcpListenerBacklog,
    pub(super) ipv4_multicast_refcnt: Mutex<Vec<(smoltcp::wire::Ipv4Address, usize)>>,
    /// Serializes configured receive-mode flags with AF_PACKET references.
    pub(super) receive_mode: Mutex<ReceiveModeState>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum PollModeRecheck {
    Current,
    Routed,
    Authoritative,
}

impl fmt::Debug for IfaceCommon {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IfaceCommon")
            .field("iface_id", &self.iface_id)
            .field("poll_deadline", &self.poll_deadline)
            .finish()
    }
}

impl IfaceCommon {
    const LOCAL_OUTPUT_POLL_BUDGET: usize = 64;
    const LOCAL_OUTPUT_TX_BACKOFF_MIN_US: u64 = 1_000;
    const LOCAL_OUTPUT_TX_BACKOFF_MAX_US: u64 = 100_000;

    pub fn new(
        iface_id: usize,
        type_: InterfaceType,
        name: String,
        mtu: usize,
        flags: InterfaceFlags,
        iface: smoltcp::iface::Interface,
    ) -> Self {
        let router_common_data = RouterEnableDeviceCommon::default();
        router_common_data
            .ip_addrs
            .write()
            .extend_from_slice(iface.ip_addrs());
        let address_metadata = iface
            .ip_addrs()
            .iter()
            .map(|cidr| AddressMetadata {
                cidr: *cidr,
                label: None,
            })
            .collect();
        IfaceCommon {
            iface_id,
            name: RwLock::new(name),
            smol_iface: Mutex::new(iface),
            sockets: Mutex::new(smoltcp::iface::SocketSet::new(Vec::new())),
            bounds: RwLock::new(Arc::new(Vec::new())),
            bound_socket_count: AtomicUsize::new(0),
            pending_routed_socket_count: AtomicUsize::new(0),
            namespace_routed_stack: AtomicBool::new(false),
            port_manager: PortManager::default(),
            poll_deadline: PollDeadline::new(),
            local_output_tx_backoff_us: AtomicU64::new(Self::LOCAL_OUTPUT_TX_BACKOFF_MIN_US),
            tx_completion_generation: AtomicU64::new(0),
            net_namespace: RwLock::new(Weak::new()),
            router_common_data,
            flags: AtomicU32::new(flags.bits()),
            mtu: AtomicUsize::new(mtu),
            type_,
            napi_struct: RwLock::new(None),
            local_input_queue: LocalInputQueue::new(),
            address_metadata: Mutex::new(address_metadata),
            bootstrap_routes: Mutex::new(Vec::new()),
            static_neighbors: RwSem::new(Vec::new()),
            tcp_close_defer: crate::net::tcp_close_defer::TcpCloseDefer::new(),
            tcp_listener_backlog: crate::net::tcp_listener_backlog::TcpListenerBacklog::new(),
            ipv4_multicast_refcnt: Mutex::new(Vec::new()),
            receive_mode: Mutex::new(ReceiveModeState {
                configured_flags: flags.bits(),
                packet_promiscuity: 0,
                packet_allmulti: 0,
            }),
        }
    }

    /// Register an active TCP listener port on this iface.
    pub fn register_tcp_listen_port(&self, port: u16, backlog: usize) {
        self.tcp_listener_backlog
            .register_tcp_listen_port(port, backlog);
    }

    /// Unregister an active TCP listener port on this iface.
    pub fn unregister_tcp_listen_port(&self, port: u16) {
        self.tcp_listener_backlog.unregister_tcp_listen_port(port);
    }

    pub fn ipv4_multicast_join_ref(
        &self,
        group: smoltcp::wire::Ipv4Address,
    ) -> Result<(), smoltcp::iface::MulticastError> {
        let mut guard = self.ipv4_multicast_refcnt.lock();
        if let Some((_, ref mut cnt)) = guard.iter_mut().find(|(g, _)| *g == group) {
            *cnt = cnt.saturating_add(1);
            return Ok(());
        }
        self.smol_iface
            .lock()
            .join_multicast_group(smoltcp::wire::IpAddress::Ipv4(group))?;
        guard.push((group, 1));
        Ok(())
    }

    pub fn ipv4_multicast_leave_ref(&self, group: smoltcp::wire::Ipv4Address) {
        let mut guard = self.ipv4_multicast_refcnt.lock();
        let Some(pos) = guard.iter().position(|(g, _)| *g == group) else {
            return;
        };
        if guard[pos].1 > 1 {
            guard[pos].1 -= 1;
            return;
        }
        guard.swap_remove(pos);
        let _ = self
            .smol_iface
            .lock()
            .leave_multicast_group(smoltcp::wire::IpAddress::Ipv4(group));
    }

    /// 驱动收包入口使用的通用丢包策略（避免驱动理解 L4 语义）。
    #[inline]
    pub fn should_drop_rx_packet(&self, packet: &[u8]) -> bool {
        self.tcp_listener_backlog
            .should_drop_backlog_full_tcp_syn_ip(packet)
    }

    pub(super) fn enqueue_local_input(&self, packet: LocalInputPacket) -> Result<(), SystemError> {
        self.local_input_queue.enqueue(packet)
    }

    pub(super) fn enqueue_routed_output(
        &self,
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_packet: &[u8],
        retry_at: smoltcp::time::Instant,
        probe_sent: bool,
    ) -> Result<(), SystemError> {
        let (packet, reservation) = self.prepare_routed_output(oif, next_hop, ip_packet)?;
        if let Err(packet) = reservation.commit_deferred_packet(packet, retry_at, probe_sent, false)
        {
            self.local_input_queue.recycle_output(packet.frame);
            return Err(SystemError::ENOBUFS);
        }
        Ok(())
    }

    pub(super) fn prepare_routed_output(
        &self,
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_packet: &[u8],
    ) -> Result<(LocalOutputPacket, LocalOutputReservation<'_>), SystemError> {
        if ip_packet.len() > self.mtu.load(Ordering::Acquire) {
            return Err(SystemError::EMSGSIZE);
        }
        let mut reservation = self
            .local_input_queue
            .reserve_output()
            .ok_or(SystemError::ENOBUFS)?;
        let mut scratch =
            LocalInputScratch::checkout(&self.local_input_queue.response_scratch, ip_packet.len())
                .ok_or(SystemError::ENOMEM)?;
        if !reservation.try_resize(scratch.capacity()) {
            return Err(SystemError::ENOBUFS);
        }
        scratch.resize(ip_packet.len()).copy_from_slice(ip_packet);
        let frame = scratch
            .take()
            .expect("a deferred routed packet owns its scratch buffer");
        Ok((
            LocalOutputPacket {
                medium: smoltcp::phy::Medium::Ip,
                meta: PacketMeta::default(),
                disposition: LocalOutputDisposition::Routed {
                    oif,
                    next_hop,
                    ip_mtu: self.mtu.load(Ordering::Acquire),
                },
                frame,
            },
            reservation,
        ))
    }

    /// Join a pending neighbor atomically if it still exists. A bucket that
    /// resolves between the optimistic presence check and commit is reported
    /// as missing; callers then use the immediate transmit path and never
    /// recreate a bucket with a stale deadline.
    pub(super) fn enqueue_existing_routed_output(
        &self,
        oif: u32,
        next_hop: smoltcp::wire::Ipv4Address,
        ip_packet: &[u8],
    ) -> Result<Option<smoltcp::time::Instant>, SystemError> {
        let pending = self
            .local_input_queue
            .output
            .lock()
            .deferred_routes
            .contains(DeferredRouteKey { oif, next_hop });
        if !pending {
            return Ok(None);
        }
        let (packet, reservation) = self.prepare_routed_output(oif, next_hop, ip_packet)?;
        match reservation.commit_existing_deferred(packet) {
            ExistingDeferredCommit::Queued(retry_at) => Ok(Some(retry_at)),
            ExistingDeferredCommit::Missing(packet, reservation) => {
                drop(reservation);
                self.local_input_queue.recycle_output(packet.frame);
                Ok(None)
            }
            ExistingDeferredCommit::Full(packet, reservation) => {
                drop(reservation);
                self.local_input_queue.recycle_output(packet.frame);
                Err(SystemError::ENOBUFS)
            }
        }
    }

    pub(super) fn enqueue_existing_deferred_output(
        &self,
        packet: LocalOutputPacket,
    ) -> ExistingDeferredEnqueue<'_> {
        let Some(mut reservation) = self.local_input_queue.reserve_output() else {
            return ExistingDeferredEnqueue::Full(packet);
        };
        if !reservation.try_resize(packet.frame.capacity()) {
            return ExistingDeferredEnqueue::Full(packet);
        }
        match reservation.commit_existing_deferred(packet) {
            ExistingDeferredCommit::Queued(retry_at) => ExistingDeferredEnqueue::Queued(retry_at),
            ExistingDeferredCommit::Missing(packet, reservation) => {
                ExistingDeferredEnqueue::Missing(packet, reservation)
            }
            ExistingDeferredCommit::Full(packet, reservation) => {
                drop(reservation);
                ExistingDeferredEnqueue::Full(packet)
            }
        }
    }

    pub(super) fn schedule_local_output(
        &self,
        retry_at: smoltcp::time::Instant,
        napi: Option<Arc<NapiStruct>>,
        netns: Option<Arc<NetNamespace>>,
    ) {
        self.namespace_routed_stack.store(true, Ordering::Release);
        self.defer_local_output_retry_at(retry_at);
        if let Some(napi) = napi {
            napi::napi_schedule(napi);
        } else if let Some(netns) = netns {
            netns.wakeup_poll_thread();
        }
    }

    pub(super) fn schedule_registered_local_output(&self, retry_at: smoltcp::time::Instant) {
        let napi = self.napi_struct.read().clone();
        let netns = napi
            .is_none()
            .then(|| self.net_namespace.read().upgrade())
            .flatten();
        self.schedule_local_output(retry_at, napi, netns);
    }

    /// Make device-backpressured output runnable after the driver reports TX
    /// completion. The per-packet deadlines remain a fallback for drivers that
    /// cannot provide a precise completion notification.
    pub(crate) fn release_tx_backpressure(&self) -> bool {
        self.tx_completion_generation
            .fetch_add(1, Ordering::Release);
        let released = self.local_input_queue.release_backpressured_outputs();
        if released {
            self.reset_local_output_tx_backoff();
        }
        released
    }

    pub(super) fn tx_completion_generation(&self) -> u64 {
        self.tx_completion_generation.load(Ordering::Acquire)
    }

    /// Close the completion-before-enqueue race without coupling queue locks
    /// to the driver. If the generation is unchanged, a later completion must
    /// observe the queued packet; if it changed, this side performs the
    /// release that the earlier notification could not see.
    pub(super) fn release_tx_backpressure_after(&self, observed_generation: u64) -> bool {
        if self.tx_completion_generation() == observed_generation {
            return false;
        }
        let released = self.local_input_queue.release_backpressured_outputs();
        if released {
            self.reset_local_output_tx_backoff();
        }
        released
    }

    pub(crate) fn notify_tx_available(&self) {
        if !self.release_tx_backpressure() {
            return;
        }
        self.schedule_registered_local_output(crate::time::Instant::now().into());
    }

    pub(crate) fn has_local_input(&self) -> bool {
        !self.local_input_queue.is_empty()
    }

    pub(super) fn release_resolved_routed_outputs(
        &self,
        interface: &mut smoltcp::iface::Interface,
        timestamp: smoltcp::time::Instant,
    ) {
        if !self.local_input_queue.has_deferred_output() {
            return;
        }
        let static_neighbors = self.static_neighbors.read();
        self.local_input_queue.release_resolved_outputs(|next_hop| {
            static_neighbors
                .iter()
                .any(|entry| entry.ip_addr == smoltcp::wire::IpAddress::Ipv4(next_hop))
                || interface
                    .is_neighbor_resolved(timestamp, smoltcp::wire::IpAddress::Ipv4(next_hop))
        });
    }

    pub(super) fn has_local_work(&self) -> bool {
        self.has_local_input() || self.local_input_queue.has_output()
    }

    pub(super) fn needs_namespace_routing(&self) -> bool {
        self.has_local_work()
            || self.namespace_routed_stack.load(Ordering::Acquire)
            || self.has_published_or_pending_sockets()
            || self.tcp_close_defer.has_pending()
    }

    /// Pairs with the publication handoff: a producer publishes `bounds`
    /// before releasing its pending reservation. Reading pending first means
    /// that observing zero synchronizes the following bound-count read with
    /// the already completed publication; observing the old nonzero value is
    /// itself sufficient to keep routed polling active.
    fn has_published_or_pending_sockets(&self) -> bool {
        let pending = self.pending_routed_socket_count.load(Ordering::Acquire);
        let published = self.bound_socket_count.load(Ordering::Acquire);
        pending != 0 || published != 0
    }

    /// Revalidates the lock-free routing-mode snapshot after both protocol
    /// locks have been acquired. This is shared by direct and NAPI polling so
    /// neither backend can process newly published output with a stale device.
    fn recheck_poll_mode(
        &self,
        netns: Option<&Arc<NetNamespace>>,
        needs_routed_poll: bool,
        authoritative_ipv4_output: bool,
    ) -> PollModeRecheck {
        if !authoritative_ipv4_output
            && netns.is_some_and(|netns| netns.router().requires_authoritative_ipv4_output())
        {
            PollModeRecheck::Authoritative
        } else if !needs_routed_poll && self.needs_namespace_routing() {
            PollModeRecheck::Routed
        } else {
            PollModeRecheck::Current
        }
    }

    pub(crate) fn begin_routed_socket_publication(&self) {
        self.pending_routed_socket_count
            .fetch_add(1, Ordering::Release);
    }

    pub(crate) fn finish_routed_socket_publication(&self) {
        let previous = self
            .pending_routed_socket_count
            .fetch_sub(1, Ordering::Release);
        debug_assert!(previous != 0);
    }

    pub(super) fn clear_namespace_routing_if_idle(&self) {
        // Reacquire the protocol lock after output draining so the fragment
        // observation and latch clear cannot race another poller's interval
        // between consuming local ingress and publishing routed output.
        let interface = self.smol_iface.lock();
        let routed_fragments_pending = interface.has_pending_egress_override();
        self.local_input_queue.clear_routed_if_idle(
            &self.namespace_routed_stack,
            &self.bound_socket_count,
            self.tcp_close_defer.has_pending(),
            routed_fragments_pending,
        );
    }

    /// Keep the routed device contract active across FIB mode changes until
    /// every saved fragment backend has completed. This publication happens
    /// while the smoltcp interface lock still protects the observation.
    pub(super) fn retain_namespace_routing_for_pending_fragments(
        &self,
        interface: &smoltcp::iface::Interface,
    ) {
        if interface.has_pending_egress_override() {
            self.namespace_routed_stack.store(true, Ordering::Release);
        }
    }

    pub(crate) fn poll_scope(&self) -> IfacePollScope {
        if self.flags().contains(InterfaceFlags::UP) {
            IfacePollScope::Full
        } else if self.needs_namespace_routing() {
            IfacePollScope::LocalOnly
        } else {
            IfacePollScope::None
        }
    }

    /// Defer removing a TCP socket from the SocketSet until it reaches Closed.
    pub fn defer_tcp_close(&self, request: crate::net::tcp_close_defer::DeferredTcpCloseRequest) {
        let now = crate::time::Instant::now().into();
        self.tcp_close_defer.defer_tcp_close(now, request);
    }

    pub fn poll<D>(&self, device: &mut D) -> bool
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        self.poll_with_authoritative_mode(device, false)
    }

    pub(super) fn poll_with_authoritative_mode<D>(
        &self,
        device: &mut D,
        force_authoritative: bool,
    ) -> bool
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        let scope = self.poll_scope();
        match scope {
            IfacePollScope::None => return false,
            IfacePollScope::LocalOnly | IfacePollScope::Full => {}
        }

        let netns = self.net_namespace();
        let authoritative_ipv4_output = force_authoritative
            || netns
                .as_ref()
                .is_some_and(|netns| netns.router().requires_authoritative_ipv4_output());
        let needs_routed_poll = self.needs_namespace_routing()
            || scope == IfacePollScope::LocalOnly
            || authoritative_ipv4_output;
        let router = if needs_routed_poll {
            netns.as_ref().map(|netns| netns.router())
        } else {
            None
        };
        let route_policy = router.as_ref().and_then(|router| {
            netns
                .as_ref()
                .map(|netns| crate::net::route::lock_output_routes(router, netns.device_list()))
        });
        let routed_this_round = route_policy.is_some();
        let owner_is_up = scope == IfacePollScope::Full;

        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        // Reservations and route-mode changes publish before SocketSet
        // mutation. Recheck after serialization and restart before processing
        // any packet if the initial backend snapshot became stale.
        let restart =
            self.recheck_poll_mode(netns.as_ref(), needs_routed_poll, authoritative_ipv4_output);
        if restart != PollModeRecheck::Current {
            drop(interface);
            drop(sockets);
            drop(route_policy);
            drop(router);
            return self.poll_with_authoritative_mode(
                device,
                force_authoritative || restart == PollModeRecheck::Authoritative,
            );
        }

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let (has_events, poll_again, deadline_rearm) = {
            let local_result = if routed_this_round
                && (self.has_local_input() || scope == IfacePollScope::LocalOnly)
            {
                let mut local_device = LocalInputDevice::new(
                    device,
                    self,
                    route_policy.as_ref().unwrap(),
                    self.iface_id as u32,
                    owner_is_up,
                    authoritative_ipv4_output,
                );
                Some(interface.poll(timestamp, &mut local_device, &mut sockets))
            } else {
                None
            };
            let poll_result = if scope == IfacePollScope::Full {
                if routed_this_round {
                    let mut routed_device = RoutedTxDevice {
                        device,
                        queue: &self.local_input_queue,
                        route_policy: route_policy.as_ref().unwrap(),
                        owner_ifindex: self.iface_id as u32,
                        owner_is_up,
                        authoritative_ipv4_output,
                    };
                    Some(interface.poll(timestamp, &mut routed_device, &mut sockets))
                } else {
                    Some(interface.poll(timestamp, device, &mut sockets))
                }
            } else {
                None
            };

            // Reclaim/advance orphaned TCP sockets after smoltcp has processed ingress.
            // If this aborts an orphan, compute poll_at afterwards so the pending RST is
            // scheduled immediately instead of waiting for an unrelated future poll.
            self.tcp_close_defer.reap_closed(timestamp, &mut sockets);

            self.release_resolved_routed_outputs(&mut interface, timestamp);
            self.retain_namespace_routing_for_pending_fragments(&interface);

            let poll_at = interface.poll_at(timestamp, &sockets);
            let (poll_again, deadline_rearm) = self.publish_poll_deadline(timestamp, poll_at);
            (
                local_result.is_some_and(|result| {
                    matches!(result, smoltcp::iface::PollResult::SocketStateChanged)
                }) || poll_result.is_some_and(|result| {
                    matches!(result, smoltcp::iface::PollResult::SocketStateChanged)
                }),
                poll_again || self.has_local_input(),
                deadline_rearm,
            )
        };

        // Publish the deadline while the smoltcp serialization locks are held,
        // but notify the namespace only after dropping them.
        drop(interface);
        drop(sockets);
        drop(route_policy);
        drop(router);
        let output_drain = self.drain_local_outputs(device, Self::LOCAL_OUTPUT_POLL_BUDGET);
        self.clear_namespace_routing_if_idle();
        self.notify_deadline_rearm(deadline_rearm);

        // 注意：不要在持有 bounds 读锁(且 irqsave)期间调用 socket.notify()。
        // 否则会形成典型锁顺序反转死锁：
        // - poll 路径：bounds.read_irqsave() -> socket.notify() -> socket.inner(RwLock)
        // - connect/bind/close 路径：socket.inner(RwLock) -> bounds.write()
        // 因此这里先快照一份 bound sockets，再逐个 notify。
        //
        // IMPORTANT: 对于 loopback 场景（如 gVisor BlockingLargeWrite 测试），始终需要唤醒所有
        // 等待的 socket。原因：smoltcp 在处理 ACK 后可能不返回 SocketStateChanged，但发送端的
        // can_send() 已经变为 true。如果只在 has_events 时唤醒，发送端会永远等待。
        // 唤醒后 socket 会重新检查条件，如果条件不满足会继续等待，所以不会造成忙等待。
        self.notify_all_bound_sockets();

        // TODO: remove closed sockets
        // let closed_sockets = self
        //     .closing_sockets
        //     .lock_irq_disabled()
        //     .extract_if(|closing_socket| closing_socket.is_closed())
        //     .collect::<Vec<_>>();
        // drop(closed_sockets);
        // `Iface::poll()` 的返回值不仅用于“这轮有没有状态变化”，还会被
        // `poll_iface_until_quiescent()` 当作“是否还需要立刻再 poll 一轮”的判据。
        //
        // smoltcp 会通过 `poll_at() == Now` 表示还有立即可推进的工作
        // （例如 loopback 二次往返、ACK/window update、仅 egress 前进等）。
        // 如果这里只返回 `has_events`，快路径会过早停止，剩余工作只能等下一次外部事件，
        // 在 blocking TCP 大包场景就会表现为 send/recv 偶发永久卡住。
        has_events || poll_again || output_drain.needs_immediate_poll()
    }

    /// Atomically classify and, when due, claim this interface's future
    /// protocol deadline.
    #[inline]
    pub fn classify_poll_deadline(&self, now_us: u64) -> DueResult {
        self.poll_deadline.classify_and_claim(now_us)
    }

    /// Restore a claimed deadline after a failed scheduler handoff, without
    /// overwriting a concurrent publisher.
    #[inline]
    pub fn restore_poll_deadline(&self, claimed_us: u64) -> bool {
        self.poll_deadline.restore_claimed_if_empty(claimed_us)
    }

    /// NAPI 使用的 bounded poll：最多处理 `budget` 个 ingress 包，然后推进一次 egress。
    ///
    /// 返回值语义：是否仍有 ingress backlog 需要继续 poll（即 budget 用尽且仍在处理包）。
    pub fn poll_napi<D>(&self, device: &mut D, budget: usize) -> napi::NapiPollResult
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        self.poll_napi_with_authoritative_mode(device, budget, false)
    }

    pub(super) fn poll_napi_with_authoritative_mode<D>(
        &self,
        device: &mut D,
        budget: usize,
        force_authoritative: bool,
    ) -> napi::NapiPollResult
    where
        D: smoltcp::phy::Device + ?Sized,
    {
        let scope = self.poll_scope();
        match scope {
            IfacePollScope::None => return napi::NapiPollResult::idle(),
            IfacePollScope::LocalOnly | IfacePollScope::Full => {}
        }

        let netns = self.net_namespace();
        let authoritative_ipv4_output = force_authoritative
            || netns
                .as_ref()
                .is_some_and(|netns| netns.router().requires_authoritative_ipv4_output());
        let needs_routed_poll = self.needs_namespace_routing()
            || scope == IfacePollScope::LocalOnly
            || authoritative_ipv4_output;
        let router = if needs_routed_poll {
            netns.as_ref().map(|netns| netns.router())
        } else {
            None
        };
        let route_policy = router.as_ref().and_then(|router| {
            netns
                .as_ref()
                .map(|netns| crate::net::route::lock_output_routes(router, netns.device_list()))
        });
        let routed_this_round = route_policy.is_some();
        let owner_is_up = scope == IfacePollScope::Full;

        let timestamp = crate::time::Instant::now().into();
        let mut sockets = self.sockets.lock();
        let mut interface = self.smol_iface.lock();

        let restart =
            self.recheck_poll_mode(netns.as_ref(), needs_routed_poll, authoritative_ipv4_output);
        if restart != PollModeRecheck::Current {
            drop(interface);
            drop(sockets);
            drop(route_policy);
            drop(router);
            return self.poll_napi_with_authoritative_mode(
                device,
                budget,
                force_authoritative || restart == PollModeRecheck::Authoritative,
            );
        }

        // 刷新 listener 缓存：必须在持有 sockets 锁的前提下进行，且不得额外分配。
        self.tcp_listener_backlog
            .refresh_listen_socket_present(&sockets);

        let mut processed = 0usize;
        let mut had_packet = false;

        // Local output is packet work too: it performs route/neighbor
        // classification and transmission rather than merely reaping TX
        // completions. Reserve half the shared NAPI budget when it is already
        // backlogged, then let either side consume unused capacity.
        let ingress_budget = if self.local_input_queue.has_ready_output(timestamp) {
            budget.div_ceil(2)
        } else {
            budget
        };

        // Reserve at most half of the first pass for namespace-local handoff,
        // then poll the hardware/device queue. If the device has no work, use
        // the remaining budget for local input. This keeps both sources
        // progressing without reducing throughput when only one is active.
        let local_first_budget = if scope == IfacePollScope::Full {
            ingress_budget.div_ceil(2)
        } else {
            ingress_budget
        };
        if routed_this_round {
            let mut local_device = LocalInputDevice::new(
                device,
                self,
                route_policy.as_ref().unwrap(),
                self.iface_id as u32,
                owner_is_up,
                authoritative_ipv4_output,
            );
            for _ in 0..local_first_budget {
                match interface.poll_ingress_single(timestamp, &mut local_device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                    }
                }
            }
        }

        let device_budget = if scope == IfacePollScope::Full {
            ingress_budget - processed
        } else {
            0
        };
        let mut device_processed = 0usize;
        if routed_this_round {
            let mut routed_device = RoutedTxDevice {
                device,
                queue: &self.local_input_queue,
                route_policy: route_policy.as_ref().unwrap(),
                owner_ifindex: self.iface_id as u32,
                owner_is_up,
                authoritative_ipv4_output,
            };
            for _ in 0..device_budget {
                match interface.poll_ingress_single(timestamp, &mut routed_device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                        device_processed += 1;
                    }
                }
            }
        } else {
            for _ in 0..device_budget {
                match interface.poll_ingress_single(timestamp, device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                        device_processed += 1;
                    }
                }
            }
        }

        let remaining = device_budget - device_processed;
        if routed_this_round && remaining > 0 && self.has_local_input() {
            let mut local_device = LocalInputDevice::new(
                device,
                self,
                route_policy.as_ref().unwrap(),
                self.iface_id as u32,
                owner_is_up,
                authoritative_ipv4_output,
            );
            for _ in 0..remaining {
                match interface.poll_ingress_single(timestamp, &mut local_device, &mut sockets) {
                    smoltcp::iface::PollIngressSingleResult::None => break,
                    smoltcp::iface::PollIngressSingleResult::PacketProcessed
                    | smoltcp::iface::PollIngressSingleResult::SocketStateChanged => {
                        had_packet = true;
                        processed += 1;
                    }
                }
            }
        }

        // 推进发送路径（smoltcp 保证 bounded work）。
        if routed_this_round && owner_is_up {
            let mut routed_device = RoutedTxDevice {
                device,
                queue: &self.local_input_queue,
                route_policy: route_policy.as_ref().unwrap(),
                owner_ifindex: self.iface_id as u32,
                owner_is_up,
                authoritative_ipv4_output,
            };
            let _ = interface.poll_egress(timestamp, &mut routed_device, &mut sockets);
        } else if routed_this_round {
            let mut local_device = LocalInputDevice::new(
                device,
                self,
                route_policy.as_ref().unwrap(),
                self.iface_id as u32,
                owner_is_up,
                authoritative_ipv4_output,
            );
            let _ = interface.poll_egress(timestamp, &mut local_device, &mut sockets);
        } else {
            let _ = interface.poll_egress(timestamp, device, &mut sockets);
        }

        self.release_resolved_routed_outputs(&mut interface, timestamp);
        self.retain_namespace_routing_for_pending_fragments(&interface);

        let poll_at = interface.poll_at(timestamp, &sockets);
        let (poll_again, deadline_rearm) = self.publish_poll_deadline(timestamp, poll_at);

        // 解锁后唤醒/通知 socket（沿用原 poll() 的 Linux-like 语义）。
        drop(interface);
        drop(sockets);
        drop(route_policy);
        drop(router);
        let output_drain = self.drain_local_outputs(device, budget - processed);
        self.clear_namespace_routing_if_idle();
        self.notify_deadline_rearm(deadline_rearm);
        self.notify_all_bound_sockets();

        // NAPI 语义：只要“还有立即可推进的工作”，就应继续留在 poll_list。
        //
        // 除了 ingress backlog 超过 budget 之外，egress/ACK 路径也可能要求立刻再次 poll：
        // smoltcp 会通过 `poll_at() == Now`（这里被 clamp 成 `Some(timestamp)`）表达这一点。
        // 如果忽略这个条件，loopback/TCP 大流量场景可能出现：
        // - 已处理完当前 ingress batch；
        // - 但仍有 ACK / window update / 后续 egress 需要立即发送；
        // - NAPI 线程却错误睡眠，直到下一次外部事件才继续推进，
        //   导致 send done 后 recv 端偶发卡住。
        napi::NapiPollResult::new(
            processed + output_drain.work_done,
            (had_packet && processed == ingress_budget)
                || poll_again
                || self.has_local_input()
                || output_drain.needs_immediate_poll(),
        )
    }

    fn drain_local_outputs<D>(&self, device: &mut D, budget: usize) -> LocalOutputDrainResult
    where
        D: SmolDevice + ?Sized,
    {
        let Some(netns) = self.net_namespace() else {
            return LocalOutputDrainResult::new(0, LocalOutputDrainState::Quiescent);
        };
        let Some(drain_guard) = self.local_input_queue.try_begin_output_drain() else {
            return if self.local_input_queue.has_output() {
                LocalOutputDrainResult::new(0, LocalOutputDrainState::BudgetExhausted)
            } else {
                LocalOutputDrainResult::new(0, LocalOutputDrainState::Contended)
            };
        };
        let mut prefer_deferred = true;
        let mut work_done = 0;
        for _ in 0..budget {
            let (mut output, mut in_flight, deferred_probe) = match self
                .local_input_queue
                .pop_ready_output(crate::time::Instant::now().into(), prefer_deferred)
            {
                LocalOutputPop::Ready(output, in_flight, deferred_probe) => {
                    (output, in_flight, deferred_probe)
                }
                LocalOutputPop::DeferredUntil(retry_at) => {
                    drop(drain_guard);
                    self.defer_local_output_retry_at(retry_at);
                    return LocalOutputDrainResult::new(
                        work_done,
                        LocalOutputDrainState::Backpressured,
                    );
                }
                LocalOutputPop::Empty => {
                    return if self.local_input_queue.finish_output_drain(drain_guard) {
                        LocalOutputDrainResult::new(
                            work_done,
                            LocalOutputDrainState::BudgetExhausted,
                        )
                    } else {
                        LocalOutputDrainResult::new(work_done, LocalOutputDrainState::Quiescent)
                    };
                }
            };
            work_done += 1;
            prefer_deferred = false;

            // Admission belongs to the actual egress interface. Atomically
            // join an existing neighbor bucket before attempting transmit so
            // neither same-owner nor cross-owner output can bypass it.
            if deferred_probe.is_none() {
                if let LocalOutputDisposition::Routed { oif, .. } = output.disposition {
                    if oif == self.iface_id as u32 {
                        match in_flight.commit_existing_deferred(output) {
                            ExistingDeferredCommit::Queued(retry_at) => {
                                self.defer_local_output_retry_at(retry_at);
                                continue;
                            }
                            ExistingDeferredCommit::Missing(packet, reservation) => {
                                output = packet;
                                in_flight = reservation;
                            }
                            ExistingDeferredCommit::Full(packet, reservation) => {
                                log::debug!(
                                    "dropping deferred output on {}: per-neighbor queue full",
                                    self.name()
                                );
                                drop(reservation);
                                self.local_input_queue.recycle_output(packet.frame);
                                continue;
                            }
                        }
                    } else if let Some(egress) = netns.device_list().get(&(oif as usize)).cloned() {
                        match egress.common().enqueue_existing_deferred_output(output) {
                            ExistingDeferredEnqueue::Queued(retry_at) => {
                                drop(in_flight);
                                egress.common().schedule_registered_local_output(retry_at);
                                continue;
                            }
                            ExistingDeferredEnqueue::Missing(packet, egress_reservation) => {
                                match transmit_admitted_routed_output(
                                    egress.as_ref(),
                                    packet,
                                    egress_reservation,
                                ) {
                                    AdmittedRoutedOutput::Sent(packet) => {
                                        drop(in_flight);
                                        self.local_input_queue.recycle_output(packet.frame);
                                    }
                                    AdmittedRoutedOutput::Queued(retry_at) => {
                                        drop(in_flight);
                                        egress.common().schedule_registered_local_output(retry_at);
                                    }
                                    AdmittedRoutedOutput::Drop(packet, error) => {
                                        log::debug!(
                                            "dropping routed output on {}: {:?}",
                                            egress.name(),
                                            error
                                        );
                                        drop(in_flight);
                                        self.local_input_queue.recycle_output(packet.frame);
                                    }
                                }
                                continue;
                            }
                            ExistingDeferredEnqueue::Full(packet) => {
                                log::debug!(
                                    "dropping deferred output on {}: egress queue full",
                                    egress.name()
                                );
                                drop(in_flight);
                                self.local_input_queue.recycle_output(packet.frame);
                                continue;
                            }
                        }
                    }
                }
            }
            let tx_generation = self.tx_completion_generation();
            match transmit_local_stack_output(
                &netns,
                self.flags().contains(InterfaceFlags::UP),
                device,
                output,
            ) {
                LocalOutputTransmitResult::Sent(output) => {
                    self.reset_local_output_tx_backoff();
                    if let Some(key) = deferred_probe {
                        self.local_input_queue.complete_deferred_probe_success(key);
                    } else if let LocalOutputDisposition::Routed { oif, next_hop, .. } =
                        output.disposition
                    {
                        self.local_input_queue.release_neighbor(oif, next_hop);
                    }
                    drop(in_flight);
                    self.local_input_queue.recycle_output(output.frame);
                }
                LocalOutputTransmitResult::Drop(output, error) => {
                    self.reset_local_output_tx_backoff();
                    log::debug!("dropping deferred output on {}: {:?}", self.name(), error);
                    if let Some(key) = deferred_probe {
                        self.local_input_queue.complete_deferred_packet_failure(key);
                    }
                    drop(in_flight);
                    self.local_input_queue.recycle_output(output.frame);
                }
                LocalOutputTransmitResult::RetrySoon(output) => {
                    let delay_us = self.next_local_output_tx_backoff_us();
                    let now: smoltcp::time::Instant = crate::time::Instant::now().into();
                    let retry_at = now + smoltcp::time::Duration::from_micros(delay_us);
                    if let Some(key) = deferred_probe {
                        // A failed physical submission waits on TX capacity,
                        // not on neighbor resolution. Return the representative
                        // to the typed TX-backpressure queue while leaving the
                        // remaining neighbor bucket eligible for another probe.
                        self.local_input_queue.complete_deferred_packet_failure(key);
                    }
                    in_flight.requeue_backpressured(output, retry_at);
                    if !self.release_tx_backpressure_after(tx_generation) {
                        self.defer_local_output_retry_at(retry_at);
                    }
                    continue;
                }
                LocalOutputTransmitResult::RetryAt {
                    packet,
                    retry_at,
                    probe_sent,
                } => {
                    self.reset_local_output_tx_backoff();
                    let LocalOutputDisposition::Routed { oif, .. } = packet.disposition else {
                        unreachable!("only routed output performs neighbor discovery");
                    };
                    if let Some(key) = deferred_probe {
                        debug_assert_eq!(oif, self.iface_id as u32);
                        match in_flight.finish_deferred_probe(packet, key, retry_at, probe_sent) {
                            Ok(resolved) => {
                                if !resolved {
                                    self.defer_local_output_retry_at(retry_at);
                                }
                            }
                            Err(packet) => {
                                self.local_input_queue.complete_deferred_packet_failure(key);
                                self.local_input_queue.recycle_output(packet.frame);
                            }
                        }
                    } else if oif == self.iface_id as u32 {
                        match in_flight.requeue_deferred(packet, retry_at, probe_sent) {
                            Ok(()) => self.defer_local_output_retry_at(retry_at),
                            Err(packet) => {
                                log::debug!(
                                    "dropping deferred output on {}: per-neighbor queue full",
                                    self.name()
                                );
                                self.local_input_queue.recycle_output(packet.frame);
                            }
                        }
                    } else {
                        // Cross-egress packets are admitted and transmitted in
                        // the branch above, while a deferred probe is always
                        // owned by this interface's neighbor bucket.
                        debug_assert!(deferred_probe.is_some());
                        drop(in_flight);
                        self.local_input_queue.recycle_output(packet.frame);
                    }
                    continue;
                }
            }
        }
        if self.local_input_queue.finish_output_drain(drain_guard) {
            LocalOutputDrainResult::new(work_done, LocalOutputDrainState::BudgetExhausted)
        } else {
            LocalOutputDrainResult::new(work_done, LocalOutputDrainState::Quiescent)
        }
    }

    pub(super) fn next_local_output_tx_backoff_us(&self) -> u64 {
        self.local_output_tx_backoff_us
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                Some(
                    current
                        .saturating_mul(2)
                        .min(Self::LOCAL_OUTPUT_TX_BACKOFF_MAX_US),
                )
            })
            .unwrap_or(Self::LOCAL_OUTPUT_TX_BACKOFF_MAX_US)
    }

    pub(super) fn reset_local_output_tx_backoff(&self) {
        self.local_output_tx_backoff_us
            .store(Self::LOCAL_OUTPUT_TX_BACKOFF_MIN_US, Ordering::Release);
    }

    pub(super) fn defer_local_output_retry_at(&self, retry_at: smoltcp::time::Instant) {
        let now_us = crate::time::Instant::now().total_micros().max(0) as u64;
        let retry_us = (retry_at.total_micros().max(0) as u64).max(now_us.saturating_add(1));
        self.publish_local_output_retry(now_us, retry_us);
    }

    pub(super) fn publish_local_output_retry(&self, now_us: u64, retry_us: u64) {
        let rearm = self.poll_deadline.publish_earlier_future(now_us, retry_us)
            == PublishResult::RearmRequired;
        self.notify_deadline_rearm(rearm);
    }

    /// Publish smoltcp's next scheduling decision while both smoltcp
    /// serialization locks are held.
    ///
    /// The returned boolean pair is `(poll_again, deadline_rearm)`.
    pub(super) fn publish_poll_deadline(
        &self,
        now: smoltcp::time::Instant,
        poll_at: Option<smoltcp::time::Instant>,
    ) -> (bool, bool) {
        match poll_at {
            Some(instant) if instant <= now => {
                self.poll_deadline.disarm();
                (true, false)
            }
            Some(instant) => {
                let now_us = now.total_micros() as u64;
                let deadline_us = instant.total_micros() as u64;
                let rearm = self.poll_deadline.publish_future(now_us, deadline_us)
                    == PublishResult::RearmRequired;
                (false, rearm)
            }
            None => {
                self.poll_deadline.disarm();
                (false, false)
            }
        }
    }

    pub(super) fn notify_deadline_rearm(&self, rearm: bool) {
        if !rearm {
            return;
        }
        if let Some(netns) = self.net_namespace() {
            netns.notify_deadline_changed();
        }
    }

    // 需要bounds储存具体的Inet Socket信息，以提供不同种类inet socket的事件分发
    pub fn bind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        let bounds = Arc::make_mut(&mut *bounds);
        if bounds.iter().any(|bound| Arc::ptr_eq(bound, &socket)) {
            return;
        }
        bounds.push(socket);
        self.bound_socket_count
            .store(bounds.len(), Ordering::Release);
    }

    pub fn unbind_socket(&self, socket: Arc<dyn InetSocket>) {
        let mut bounds = self.bounds.write();
        let bounds = Arc::make_mut(&mut *bounds);
        if let Some(index) = bounds.iter().position(|s| Arc::ptr_eq(s, &socket)) {
            bounds.remove(index);
            self.bound_socket_count
                .store(bounds.len(), Ordering::Release);
            // log::debug!("unbind socket success");
        }
    }

    /// Notify all bound sockets unconditionally.
    /// This is used after listener shutdown to ensure all client sockets
    /// are woken up even if the interface poll didn't detect any events.
    pub fn notify_all_bound_sockets(&self) {
        // Take one coherent snapshot before dropping the lock. Iterating by index while
        // repeatedly releasing the lock can skip sockets when a concurrent close removes
        // an earlier element and shifts the Vec to the left.
        // Use a single read-side critical section. A size pass followed by a copy pass
        // can be forced behind the stream of close-side writers between acquisitions,
        // delaying network progress long enough for poll waiters to time out.
        // Clone only the outer Arc while IRQs are disabled.  Mutations use
        // Arc::make_mut(), so this remains a coherent snapshot without an
        // O(n) allocation in the polling hot path.
        let sockets = self.bounds.read_irqsave().clone();
        for sock in sockets.iter() {
            sock.notify();
            let _woke = sock.wait_queue().wakeup(Some(ProcessState::Blocked(true)));
        }
    }

    pub fn ipv4_addr(&self) -> Option<Ipv4Addr> {
        self.smol_iface.lock().ipv4_addr()
    }

    pub fn ip_addrs(&self) -> RwSemReadGuard<'_, Vec<smoltcp::wire::IpCidr>> {
        self.router_common_data.ip_addrs.read()
    }

    pub fn prefix_len(&self) -> Option<u8> {
        self.smol_iface
            .lock()
            .ip_addrs()
            .first()
            .map(|ip_addr| ip_addr.prefix_len())
    }

    pub fn net_namespace(&self) -> Option<Arc<NetNamespace>> {
        self.net_namespace.read().upgrade()
    }

    pub fn set_net_namespace(&self, ns: Arc<NetNamespace>) {
        *self.net_namespace.write() = Arc::downgrade(&ns);
        self.sockets.lock().set_udp_ingress_handler(Some(Arc::new(
            crate::net::socket::inet::datagram::udp_bindings::NetnsUdpIngress::new(
                &ns,
                self.iface_id,
            ),
        )));
    }

    pub fn clear_net_namespace(&self) {
        *self.net_namespace.write() = Weak::new();
        self.sockets.lock().set_udp_ingress_handler(None);
    }

    /// Runs a construction-time mutation while preventing namespace
    /// publication from starting. This is the lifecycle barrier shared by
    /// all bootstrap state owned by an interface.
    pub(crate) fn with_unpublished<T>(
        &self,
        mutation: impl FnOnce() -> Result<T, SystemError>,
    ) -> Result<T, SystemError> {
        let namespace = self.net_namespace.read();
        if namespace.upgrade().is_some() {
            return Err(SystemError::EBUSY);
        }
        mutation()
    }

    pub fn name(&self) -> String {
        self.name.read().clone()
    }

    /// Read the interface name without cloning it.
    ///
    /// The callback runs while the name read lock is held and must not acquire
    /// address-metadata, FIB, or smoltcp locks. This narrow API lets control
    /// plane prepare phases copy into already-reserved storage without hiding
    /// an infallible `String` allocation in a read accessor.
    pub(crate) fn with_iface_name<R>(&self, f: impl FnOnce(&str) -> R) -> R {
        let name = self.name.read();
        f(name.as_str())
    }

    pub fn set_name(&self, name: String) {
        *self.name.write() = name;
    }

    /// Publish one prepared interface-name/address-label generation.
    ///
    /// Both values are moved into place without allocation. The lock order is
    /// intentionally `address_metadata -> name`; callers must never acquire
    /// these locks in the reverse order.
    pub(crate) fn publish_name_and_address_metadata(
        &self,
        name: String,
        metadata: Vec<AddressMetadata>,
    ) {
        let mut current_metadata = self.address_metadata.lock();
        let mut current_name = self.name.write();
        *current_metadata = metadata;
        *current_name = name;
    }

    pub fn flags(&self) -> InterfaceFlags {
        InterfaceFlags::from_bits_truncate(self.flags.load(Ordering::Acquire))
    }

    pub(crate) fn prepare_configured_flags(
        &self,
        requested: InterfaceFlags,
        change_mask: InterfaceFlags,
    ) -> Result<PreparedConfiguredFlags, SystemError> {
        let state = self.receive_mode.lock();
        let old = InterfaceFlags::from_bits_truncate(state.configured_flags);
        let configured = (state.configured_flags & !change_mask.bits())
            | (requested.bits() & change_mask.bits());

        Self::total_receive_mode_count(
            state.packet_promiscuity,
            configured & InterfaceFlags::PROMISC.bits() != 0,
        )?;
        Self::total_receive_mode_count(
            state.packet_allmulti,
            configured & InterfaceFlags::ALLMULTI.bits() != 0,
        )?;

        Ok(PreparedConfiguredFlags {
            old,
            new: InterfaceFlags::from_bits_truncate(configured),
        })
    }

    /// Publishes a flag update whose fallible validation completed while RTNL
    /// was held. RTNL serializes configured-flag writers; packet-socket
    /// receive-mode references remain independent and are folded into the
    /// effective flags under the same lock.
    pub(crate) fn publish_configured_flags(&self, prepared: PreparedConfiguredFlags) {
        let mut state = self.receive_mode.lock();
        debug_assert_eq!(
            InterfaceFlags::from_bits_truncate(state.configured_flags),
            prepared.old_flags()
        );
        state.configured_flags = prepared.new_flags().bits();
        self.publish_effective_flags(&state);
    }

    pub fn link_flags_snapshot(&self) -> Result<LinkFlagsSnapshot, SystemError> {
        let state = self.receive_mode.lock();
        let configured = InterfaceFlags::from_bits_truncate(state.configured_flags);
        Ok(LinkFlagsSnapshot {
            configured,
            promiscuity: Self::total_receive_mode_count(
                state.packet_promiscuity,
                configured.contains(InterfaceFlags::PROMISC),
            )?,
            allmulti: Self::total_receive_mode_count(
                state.packet_allmulti,
                configured.contains(InterfaceFlags::ALLMULTI),
            )?,
        })
    }

    pub fn configured_flags(&self) -> InterfaceFlags {
        InterfaceFlags::from_bits_truncate(self.receive_mode.lock().configured_flags)
    }

    pub fn type_(&self) -> InterfaceType {
        self.type_
    }

    pub fn mtu(&self) -> usize {
        self.mtu.load(Ordering::Relaxed)
    }

    pub fn set_mtu(&self, mtu: usize) {
        self.mtu.store(mtu, Ordering::Relaxed);
    }

    pub(crate) fn address_metadata(&self) -> &Mutex<Vec<AddressMetadata>> {
        &self.address_metadata
    }

    /// Stages a route before publication. Holding the namespace read guard
    /// closes the race with `set_net_namespace()` during registration.
    pub(crate) fn stage_bootstrap_route(&self, route: BootstrapRoute) -> Result<(), SystemError> {
        self.with_unpublished(|| {
            let expected_oif = u32::try_from(self.iface_id).map_err(|_| SystemError::EOVERFLOW)?;
            if route.oif != expected_oif {
                return Err(SystemError::EINVAL);
            }

            let mut routes = self.bootstrap_routes.lock();
            if routes.contains(&route) {
                return Err(SystemError::EEXIST);
            }
            routes.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
            routes.push(route);
            Ok(())
        })
    }

    /// Transfers unpublished routes to netns registration without cloning.
    pub(crate) fn take_bootstrap_routes(&self) -> Vec<BootstrapRoute> {
        core::mem::take(&mut *self.bootstrap_routes.lock())
    }

    /// Restores the hand-off buffer when netns registration rolls back.
    pub(crate) fn restore_bootstrap_routes(&self, routes: Vec<BootstrapRoute>) {
        let mut staged = self.bootstrap_routes.lock();
        debug_assert!(staged.is_empty());
        *staged = routes;
    }

    pub fn static_neighbors(&self) -> RwSemReadGuard<'_, Vec<StaticNeighborEntry>> {
        self.static_neighbors.read()
    }

    pub fn set_static_neighbor(&self, entry: StaticNeighborEntry) {
        let ip_addr = entry.ip_addr;
        let mut neighbors = self.static_neighbors.write();
        if let Some(existing) = neighbors
            .iter_mut()
            .find(|existing| existing.ip_addr == entry.ip_addr)
        {
            *existing = entry;
        } else {
            neighbors.push(entry);
        }
        drop(neighbors);
        let smoltcp::wire::IpAddress::Ipv4(next_hop) = ip_addr else {
            return;
        };
        if self
            .local_input_queue
            .release_neighbor(self.iface_id as u32, next_hop)
        {
            self.schedule_registered_local_output(crate::time::Instant::now().into());
        }
    }

    pub fn remove_static_neighbor(&self, ip_addr: smoltcp::wire::IpAddress) -> bool {
        let mut neighbors = self.static_neighbors.write();
        let before = neighbors.len();
        neighbors.retain(|existing| existing.ip_addr != ip_addr);
        neighbors.len() != before
    }

    pub fn adjust_promiscuity(&self, inc: i32) -> Result<(), SystemError> {
        self.adjust_receive_mode(InterfaceFlags::PROMISC, inc)
    }

    pub fn adjust_allmulti(&self, inc: i32) -> Result<(), SystemError> {
        self.adjust_receive_mode(InterfaceFlags::ALLMULTI, inc)
    }

    pub(super) fn adjust_receive_mode(
        &self,
        flag: InterfaceFlags,
        inc: i32,
    ) -> Result<(), SystemError> {
        let mut state = self.receive_mode.lock();
        let old = if flag == InterfaceFlags::PROMISC {
            state.packet_promiscuity
        } else if flag == InterfaceFlags::ALLMULTI {
            state.packet_allmulti
        } else {
            return Err(SystemError::EINVAL);
        };
        let new = match inc {
            1 => old.checked_add(1).ok_or(SystemError::EOVERFLOW)?,
            -1 => old.checked_sub(1).ok_or(SystemError::EINVAL)?,
            _ => return Err(SystemError::EINVAL),
        };
        let configured = state.configured_flags & flag.bits() != 0;
        Self::total_receive_mode_count(new, configured)?;

        if flag == InterfaceFlags::PROMISC {
            state.packet_promiscuity = new;
        } else {
            state.packet_allmulti = new;
        }
        self.publish_effective_flags(&state);
        Ok(())
    }

    pub(super) fn total_receive_mode_count(
        packet: u32,
        configured: bool,
    ) -> Result<u32, SystemError> {
        packet
            .checked_add(u32::from(configured))
            .ok_or(SystemError::EOVERFLOW)
    }

    pub(super) fn publish_effective_flags(&self, state: &ReceiveModeState) {
        let mut effective = state.configured_flags;
        if state.packet_promiscuity != 0 {
            effective |= InterfaceFlags::PROMISC.bits();
        }
        if state.packet_allmulti != 0 {
            effective |= InterfaceFlags::ALLMULTI.bits();
        }
        self.flags.store(effective, Ordering::Release);
    }
}
