use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::AtomicU64;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize};
use system_error::SystemError;

use crate::filesystem::vfs::{fasync::FAsyncItems, vcore::generate_inode_id, InodeId};
use crate::libs::mutex::Mutex;
use crate::libs::rwsem::RwSem;
use crate::libs::wait_queue::WaitQueue;
use crate::net::socket::common::EPollItems;
use crate::net::socket::inet::common::port::TcpPortOwner;
use crate::net::socket::inet::common::SocketDeviceBinding;
use crate::net::socket::Socket;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;

use super::constants;
use super::inner;

type EP = crate::filesystem::epoll::EPollEventType;

#[derive(Debug)]
pub struct TcpSocketOptions {
    pub(crate) nagle_enabled: AtomicBool,
    pub(crate) congestion_control:
        crate::libs::spinlock::SpinLock<smoltcp::socket::tcp::CongestionControl>,
    /// Updates are serialized with bind/connect by `TcpSocket::inner`.
    pub(crate) ipv6_only: AtomicBool,
    /// SO_SNDTIMEO (microseconds). u64::MAX means "no timeout".
    pub(crate) send_timeout_us: AtomicU64,
    /// SO_RCVTIMEO (microseconds). u64::MAX means "no timeout".
    pub(crate) recv_timeout_us: AtomicU64,

    /// TCP_INQ: whether to report inq bytes via recvmsg cmsg.
    pub(crate) tcp_inq_enabled: AtomicBool,
    /// SO_TIMESTAMP: whether to attach SCM_TIMESTAMP on recvmsg.
    pub(crate) so_timestamp_enabled: AtomicBool,
    /// IP_MTU_DISCOVER: PMTU discovery strategy.
    pub(crate) ip_mtu_discover: core::sync::atomic::AtomicI32,
    /// IPV6_MTU_DISCOVER is independent of the IPv4 option on dual-stack sockets.
    pub(crate) ipv6_mtu_discover: AtomicI32,

    pub(crate) send_buf_size: AtomicUsize,
    pub(crate) recv_buf_size: AtomicUsize,
    /// SO_RCVLOWAT
    pub(crate) rcvlowat: AtomicI32,

    /// TCP_MAXSEG
    pub(crate) tcp_max_seg: AtomicUsize,
    /// TCP_DEFER_ACCEPT
    pub(crate) tcp_defer_accept: AtomicI32,
    /// TCP_SYNCNT
    pub(crate) tcp_syncnt: AtomicI32,
    /// TCP_WINDOW_CLAMP
    pub(crate) tcp_window_clamp: AtomicUsize,
    /// TCP_USER_TIMEOUT
    pub(crate) tcp_user_timeout: AtomicI32,

    /// SO_ATTACH_FILTER: whether a filter is attached.
    pub(crate) so_filter_attached: AtomicBool,
    /// SO_BROADCAST
    pub(crate) so_broadcast: AtomicBool,
    /// SO_PASSCRED
    pub(crate) so_passcred: AtomicBool,
    /// SO_NO_CHECK
    pub(crate) so_no_check: AtomicBool,
    /// TCP_CORK
    pub(crate) tcp_cork: AtomicBool,
    /// TCP_QUICKACK
    pub(crate) tcp_quickack: AtomicBool,
    /// SO_KEEPALIVE
    pub(crate) so_keepalive: AtomicBool,

    /// TCP_KEEPIDLE (seconds)
    pub(crate) tcp_keepidle_secs: AtomicI32,
    /// TCP_KEEPINTVL (seconds)
    pub(crate) tcp_keepintvl_secs: AtomicI32,
    /// TCP_KEEPCNT
    pub(crate) tcp_keepcnt: AtomicI32,

    /// SO_LINGER
    pub(crate) linger_onoff: AtomicI32,
    pub(crate) linger_linger: AtomicI32,

    /// IP_MULTICAST_TTL
    pub(crate) ip_multicast_ttl: AtomicI32,
    /// IP_MULTICAST_LOOP
    pub(crate) ip_multicast_loop: AtomicBool,

    /// SO_OOBINLINE
    pub(crate) so_oobinline: AtomicBool,
    /// TCP_LINGER2 (seconds; 0 means default)
    pub(crate) tcp_linger2_secs: AtomicI32,
}

impl TcpSocketOptions {
    fn new() -> Self {
        Self {
            nagle_enabled: AtomicBool::new(true),
            congestion_control: crate::libs::spinlock::SpinLock::new(
                smoltcp::socket::tcp::CongestionControl::Reno,
            ),
            ipv6_only: AtomicBool::new(false),
            send_timeout_us: AtomicU64::new(u64::MAX),
            recv_timeout_us: AtomicU64::new(u64::MAX),

            tcp_inq_enabled: AtomicBool::new(false),
            so_timestamp_enabled: AtomicBool::new(false),
            // Default: IP_PMTUDISC_WANT (1)
            ip_mtu_discover: core::sync::atomic::AtomicI32::new(1),
            ipv6_mtu_discover: AtomicI32::new(1),
            send_buf_size: AtomicUsize::new(inner::DEFAULT_TX_BUF_SIZE),
            recv_buf_size: AtomicUsize::new(inner::DEFAULT_RX_BUF_SIZE),
            rcvlowat: AtomicI32::new(1),

            tcp_max_seg: AtomicUsize::new(constants::DEFAULT_TCP_MSS), // Default MSS
            tcp_defer_accept: AtomicI32::new(0),
            tcp_syncnt: AtomicI32::new(constants::DEFAULT_TCP_SYNCNT), // Default 6
            tcp_window_clamp: AtomicUsize::new(0),
            tcp_user_timeout: AtomicI32::new(0),
            so_filter_attached: AtomicBool::new(false),
            so_broadcast: AtomicBool::new(false),
            so_passcred: AtomicBool::new(false),
            so_no_check: AtomicBool::new(false),
            tcp_cork: AtomicBool::new(false),
            tcp_quickack: AtomicBool::new(true),
            so_keepalive: AtomicBool::new(false),

            tcp_keepidle_secs: AtomicI32::new(2 * 60 * 60),
            tcp_keepintvl_secs: AtomicI32::new(75),
            tcp_keepcnt: AtomicI32::new(9),

            linger_onoff: AtomicI32::new(0),
            linger_linger: AtomicI32::new(0),

            so_oobinline: AtomicBool::new(false),
            tcp_linger2_secs: AtomicI32::new(0),

            ip_multicast_ttl: AtomicI32::new(constants::IP_MULTICAST_TTL_DEFAULT),
            ip_multicast_loop: AtomicBool::new(constants::IP_MULTICAST_LOOP_DEFAULT),
        }
    }
}

#[cast_to([sync] Socket)]
#[derive(Debug)]
pub struct TcpSocket {
    data: crate::libs::lazy_init::Lazy<crate::cgroup::memory::owned::OwnedBox<TcpSocketData>>,
}

/// Fully born but unpublished accepted FD entity. No temporary protocol rings
/// are allocated: the existing pending child supplies its Inner at commit.
pub(super) struct PreparedAcceptedSocket {
    socket: Arc<TcpSocket>,
}

impl PreparedAcceptedSocket {
    pub(super) fn memory(&self) -> &crate::net::socket::buffer::SocketMemory {
        &self.socket.memory
    }

    pub(super) fn commit(self, established: inner::Established) -> Arc<TcpSocket> {
        let socket = self.socket;
        let (send, recv, nagle, cc, keepalive) = established.with(|protocol| {
            (
                protocol.send_buffer_limit(),
                protocol.receive_buffer_limit(),
                protocol.nagle_enabled(),
                protocol.congestion_control(),
                protocol.keep_alive(),
            )
        });
        socket
            .options
            .send_buf_size
            .store(send, core::sync::atomic::Ordering::Relaxed);
        socket
            .options
            .recv_buf_size
            .store(recv, core::sync::atomic::Ordering::Relaxed);
        socket
            .options
            .nagle_enabled
            .store(nagle, core::sync::atomic::Ordering::Relaxed);
        *socket.options.congestion_control.lock_irqsave() = cc;
        socket
            .options
            .so_keepalive
            .store(keepalive.is_some(), core::sync::atomic::Ordering::Relaxed);
        let policies = established.pmtu_policies();
        socket
            .options
            .ip_mtu_discover
            .store(policies[0] as i32, core::sync::atomic::Ordering::Relaxed);
        socket
            .options
            .ipv6_mtu_discover
            .store(policies[1] as i32, core::sync::atomic::Ordering::Relaxed);
        socket
            .inner
            .write()
            .replace(inner::Inner::Established(established));
        socket
    }
}

/// The actual socket entity is freed with the last strong reference. Weak
/// protocol identities retain only the thin Arc header, not this charged body.
#[derive(Debug)]
pub struct TcpSocketData {
    pub(crate) memory: crate::net::socket::buffer::SocketMemory,
    pub(crate) inner: RwSem<Option<inner::Inner>>,
    pub(crate) shutdown: AtomicUsize,
    /// If SHUT_WR is requested while `cork_buf` still contains bytes that have not been
    /// handed to the underlying TCP stack, defer sending FIN until those bytes are flushed.
    pub(crate) send_fin_deferred: AtomicBool,
    pub(crate) nonblock: AtomicBool,
    pub(crate) wait_queue: WaitQueue,
    pub(crate) inode_id: InodeId,
    pub(crate) open_files: AtomicUsize,
    pub(crate) fsnotify_watches: AtomicUsize,
    pub(crate) self_ref: Weak<TcpSocket>,
    pub(crate) pollee: AtomicUsize,
    pub(crate) netns: Arc<NetNamespace>,
    pub(crate) device_binding: Arc<SocketDeviceBinding>,
    /// SO_REUSEADDR and bind/protocol ownership share one namespace authority.
    pub(crate) port_owner: TcpPortOwner,
    pub(crate) epoll_items: EPollItems,
    pub(crate) fasync_items: FAsyncItems,
    pub(crate) options: TcpSocketOptions,
    pub(crate) cork_buf: Mutex<Vec<u8>>,
    /// Serializes readers while a receive transaction temporarily drops the interface-wide
    /// SocketSet lock to copy data to userspace.
    pub(crate) recv_lock: Mutex<()>,
    pub(crate) resize_lock: Mutex<()>,
    pub(crate) cork_flush_in_progress: AtomicBool,
    pub(crate) cork_timer_active: AtomicBool,
    pub(crate) ip_version: smoltcp::wire::IpVersion,
}

impl TcpSocket {
    pub(super) fn prepare_bound_slot(
        &self,
    ) -> Result<crate::net::tcp_bounds::BoundSlotReservation, SystemError> {
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            match self.netns.tcp_stack().reserve_bound_slot(&creator) {
                Ok(reserved) => return Ok(reserved),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "Prepared owner, provenance and socket options have independent cancellation lifetimes"
    )]
    fn new_common(
        inner: Option<inner::Inner>,
        nonblock: bool,
        netns: Arc<NetNamespace>,
        pollee_bits: usize,
        ip_version: smoltcp::wire::IpVersion,
        me: &Weak<Self>,
        memory: crate::net::socket::buffer::SocketMemory,
        prepared_owner: Option<(TcpPortOwner, Arc<SocketDeviceBinding>)>,
    ) -> TcpSocketData {
        let port_owner = match prepared_owner.as_ref() {
            Some((owner, _)) => owner.clone(),
            None => match &inner {
                Some(inner::Inner::Established(est)) => est.port_owner(),
                _ => netns
                    .tcp_ports()
                    .new_owner(Arc::new(SocketDeviceBinding::from_ifindex(0))),
            },
        };
        let device_binding =
            prepared_owner.map_or_else(|| port_owner.device_binding(), |(_, device)| device);
        let options = TcpSocketOptions::new();
        if let Some(inner) = &inner {
            if let Some((nagle, cc)) =
                inner.with_socket(|socket| (socket.nagle_enabled(), socket.congestion_control()))
            {
                options
                    .nagle_enabled
                    .store(nagle, core::sync::atomic::Ordering::Relaxed);
                *options.congestion_control.lock_irqsave() = cc;
            }
        }
        if let Some(inner::Inner::Established(established)) = &inner {
            let policies = established.pmtu_policies();
            options
                .ip_mtu_discover
                .store(policies[0] as i32, core::sync::atomic::Ordering::Relaxed);
            options
                .ipv6_mtu_discover
                .store(policies[1] as i32, core::sync::atomic::Ordering::Relaxed);
            let (version, policy) = established.with(|socket| {
                (
                    socket
                        .local_endpoint()
                        .map_or(ip_version, |local| local.addr.version()),
                    socket.pmtu_discover(),
                )
            });
            let atomic = match version {
                smoltcp::wire::IpVersion::Ipv4 => &options.ip_mtu_discover,
                smoltcp::wire::IpVersion::Ipv6 => &options.ipv6_mtu_discover,
            };
            atomic.store(policy as i32, core::sync::atomic::Ordering::Relaxed);
        }
        TcpSocketData {
            memory,
            inner: RwSem::new(inner),
            shutdown: AtomicUsize::new(0),
            send_fin_deferred: AtomicBool::new(false),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            fsnotify_watches: AtomicUsize::new(0),
            self_ref: me.clone(),
            pollee: AtomicUsize::new(pollee_bits),
            netns,
            device_binding,
            port_owner,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            options,
            cork_buf: Mutex::new(Vec::new()),
            recv_lock: Mutex::new(()),
            resize_lock: Mutex::new(()),
            cork_flush_in_progress: AtomicBool::new(false),
            cork_timer_active: AtomicBool::new(false),
            ip_version,
        }
    }

    pub fn new(nonblock: bool, ver: smoltcp::wire::IpVersion) -> Result<Arc<Self>, SystemError> {
        let netns = ProcessManager::current_netns();
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            let task = ProcessManager::current_pcb();
            let account = crate::cgroup::memory::account_for_task(&task);
            let creator = crate::cgroup::memory::object_for_task(&task);
            let prepared = (|| {
                let memory = crate::net::socket::buffer::SocketMemory::prepare(&account, creator)?;
                let data = crate::cgroup::memory::owned::OwnedBox::<TcpSocketData>::prepare(
                    &memory.creator,
                    crate::cgroup::memory::accounting::KernelByteKind::SlabUnreclaimable,
                )?
                .try_allocate()
                .map_err(|_| SystemError::ENOMEM)?;
                let init = inner::Init::new(ver, &memory)?;
                let socket = Arc::try_new(Self {
                    data: crate::libs::lazy_init::Lazy::new(),
                })
                .map_err(|_| SystemError::ENOMEM)?;
                Ok::<_, crate::mm::allocation::MemoryAllocationError>((memory, data, init, socket))
            })();
            match prepared {
                Ok((memory, data, init, socket)) => {
                    // The only Weak is cloned into this private body. Nothing
                    // registers, upgrades, or publishes it before Lazy.init.
                    socket.data.init(data.init(Self::new_common(
                        Some(inner::Inner::Init(init)),
                        nonblock,
                        netns,
                        0,
                        ver,
                        &Arc::downgrade(&socket),
                        memory,
                        None,
                    )));
                    return Ok(socket);
                }
                Err(error) => error.recover(&mut recovery)?,
            }
        }
    }

    pub(super) fn prepare_accepted(
        nonblock: bool,
        netns: Arc<NetNamespace>,
        ip_version: smoltcp::wire::IpVersion,
        memory: crate::net::socket::buffer::SocketMemory,
        binding: &crate::net::socket::inet::common::port::PreparedTcpAcceptBinding,
    ) -> Result<PreparedAcceptedSocket, crate::mm::allocation::MemoryAllocationError> {
        let data = crate::cgroup::memory::owned::OwnedBox::<TcpSocketData>::prepare(
            &memory.creator,
            crate::cgroup::memory::accounting::KernelByteKind::SlabUnreclaimable,
        )?
        .try_allocate()
        .map_err(|_| SystemError::ENOMEM)?;
        let socket = Arc::try_new(Self {
            data: crate::libs::lazy_init::Lazy::new(),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        socket.data.init(data.init(Self::new_common(
            None,
            nonblock,
            netns,
            (EP::EPOLLIN.bits() | EP::EPOLLOUT.bits()) as usize,
            ip_version,
            &Arc::downgrade(&socket),
            memory,
            Some((binding.owner.clone(), binding.device.clone())),
        )));
        Ok(PreparedAcceptedSocket { socket })
    }

    #[inline]
    pub fn is_listening(&self) -> bool {
        matches!(self.inner.read().as_ref(), Some(inner::Inner::Listening(_)))
    }

    /// Binding an unbound protocol value is the only Init transition that
    /// needs a new table slot. Prepare the shared array before acquiring the
    /// transaction's inner guard; a racing bind may make preparation obsolete.
    pub(super) fn prepare_new_handle_capacity(&self) -> Result<(), SystemError> {
        let needs_handle = || {
            matches!(
                self.inner.read().as_ref(),
                Some(inner::Inner::Init(inner::Init::Unbound(_)))
            )
        };
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        while needs_handle() {
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            match self.netns.tcp_stack().prepare_socket_capacity(&creator) {
                Ok(()) => return Ok(()),
                Err(_) if !needs_handle() => return Ok(()),
                Err(error) => error.recover(&mut recovery)?,
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn inq_enabled(&self) -> bool {
        self.options
            .tcp_inq_enabled
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Returns an owned interface snapshot for paths that need to progress smoltcp.
    ///
    /// The `inner` guard is always released before this function returns. Callers may
    /// therefore poll the interface without allowing `TcpStack::poll() -> notify()` to
    /// recursively acquire `inner` while an outer read guard is still alive.
    pub(super) fn stack_poll_snapshot(&self) -> Option<Arc<crate::net::tcp_stack::TcpStack>> {
        let inner = self.inner.read();
        match inner.as_ref() {
            None => None,
            Some(inner) => inner.stack().cloned(),
        }
    }

    #[inline]
    pub(crate) fn timestamp_enabled(&self) -> bool {
        self.options
            .so_timestamp_enabled
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    pub fn recv_queue_len(&self) -> usize {
        self.inner
            .read()
            .as_ref()
            .map(|inner| match inner {
                inner::Inner::Closed(_) => 0,
                _ => inner.with_socket(|s| s.recv_queue()).unwrap_or(0),
            })
            .unwrap_or(0)
    }

    #[inline]
    pub(crate) fn clamp_usize_to_i32(v: usize) -> i32 {
        if v > i32::MAX as usize {
            i32::MAX
        } else {
            v as i32
        }
    }
    pub fn is_nonblock(&self) -> bool {
        self.nonblock.load(core::sync::atomic::Ordering::Relaxed)
    }

    // ========== Socket option helper methods (pub(crate)) ==========
    // These methods provide internal access to private socket option fields.

    #[inline]
    pub(crate) fn send_timeout_us(&self) -> &AtomicU64 {
        &self.options.send_timeout_us
    }

    #[inline]
    pub(crate) fn recv_timeout_us(&self) -> &AtomicU64 {
        &self.options.recv_timeout_us
    }

    #[inline]
    pub(crate) fn so_timestamp_enabled(&self) -> &AtomicBool {
        &self.options.so_timestamp_enabled
    }

    #[inline]
    pub(crate) fn tcp_inq_enabled(&self) -> &AtomicBool {
        &self.options.tcp_inq_enabled
    }

    #[inline]
    pub(crate) fn tcp_max_seg(&self) -> &AtomicUsize {
        &self.options.tcp_max_seg
    }

    #[inline]
    pub(crate) fn tcp_defer_accept(&self) -> &AtomicI32 {
        &self.options.tcp_defer_accept
    }

    #[inline]
    pub(crate) fn tcp_syncnt(&self) -> &AtomicI32 {
        &self.options.tcp_syncnt
    }

    #[inline]
    pub(crate) fn tcp_window_clamp(&self) -> &AtomicUsize {
        &self.options.tcp_window_clamp
    }

    #[inline]
    pub(crate) fn tcp_user_timeout(&self) -> &AtomicI32 {
        &self.options.tcp_user_timeout
    }

    #[inline]
    pub(crate) fn ip_mtu_discover(&self) -> &AtomicI32 {
        &self.options.ip_mtu_discover
    }

    #[inline]
    pub(crate) fn so_filter_attached(&self) -> &AtomicBool {
        &self.options.so_filter_attached
    }

    #[inline]
    pub(crate) fn so_broadcast(&self) -> &AtomicBool {
        &self.options.so_broadcast
    }

    #[inline]
    pub(crate) fn so_passcred(&self) -> &AtomicBool {
        &self.options.so_passcred
    }

    #[inline]
    pub(crate) fn so_no_check(&self) -> &AtomicBool {
        &self.options.so_no_check
    }

    #[inline]
    pub(crate) fn tcp_quickack_enabled(&self) -> &AtomicBool {
        &self.options.tcp_quickack
    }

    #[inline]
    pub(crate) fn so_keepalive_enabled(&self) -> &AtomicBool {
        &self.options.so_keepalive
    }

    #[inline]
    pub(crate) fn tcp_keepidle_secs(&self) -> &AtomicI32 {
        &self.options.tcp_keepidle_secs
    }

    #[inline]
    pub(crate) fn tcp_keepintvl_secs(&self) -> &AtomicI32 {
        &self.options.tcp_keepintvl_secs
    }

    #[inline]
    pub(crate) fn tcp_keepcnt(&self) -> &AtomicI32 {
        &self.options.tcp_keepcnt
    }

    #[inline]
    pub(crate) fn linger_onoff(&self) -> &AtomicI32 {
        &self.options.linger_onoff
    }

    #[inline]
    pub(crate) fn linger_linger(&self) -> &AtomicI32 {
        &self.options.linger_linger
    }

    #[inline]
    pub(crate) fn ip_multicast_ttl(&self) -> &AtomicI32 {
        &self.options.ip_multicast_ttl
    }

    #[inline]
    pub(crate) fn ip_multicast_loop(&self) -> &AtomicBool {
        &self.options.ip_multicast_loop
    }

    #[inline]
    pub(crate) fn so_oobinline_enabled(&self) -> &AtomicBool {
        &self.options.so_oobinline
    }

    #[inline]
    pub(crate) fn tcp_linger2_secs(&self) -> &AtomicI32 {
        &self.options.tcp_linger2_secs
    }

    #[inline]
    pub(crate) fn send_buf_size(&self) -> &AtomicUsize {
        &self.options.send_buf_size
    }

    #[inline]
    pub(crate) fn recv_buf_size(&self) -> &AtomicUsize {
        &self.options.recv_buf_size
    }

    #[inline]
    pub(crate) fn send_buf_size_loaded(&self) -> usize {
        self.options
            .send_buf_size
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn recv_buf_size_loaded(&self) -> usize {
        self.options
            .recv_buf_size
            .load(core::sync::atomic::Ordering::Relaxed)
    }

    pub(crate) fn update_send_buffer(&self, size: usize) -> Result<(), SystemError> {
        self.update_socket_buffer(size, true)
    }

    pub(crate) fn update_receive_buffer(&self, size: usize) -> Result<(), SystemError> {
        // The protocol's serial-number/window contract supports at most 1GiB.
        if size as u64 > (1u64 << 30) {
            return Err(SystemError::ENOBUFS);
        }
        self.update_socket_buffer(size, false)
    }

    fn update_socket_buffer(&self, size: usize, send: bool) -> Result<(), SystemError> {
        let mut recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
        loop {
            // Domain lookup precedes even the task-only resize mutex.
            let creator = crate::cgroup::memory::object_for_task(&ProcessManager::current_pcb());
            let operation = self.resize_lock.lock();
            let capacity = {
                let inner = self.inner.read();
                match inner.as_ref() {
                    Some(inner::Inner::Closed(_) | inner::Inner::Listening(_)) | None => 0,
                    Some(inner) => inner
                        .with_socket(|socket| {
                            if send {
                                socket.send_capacity()
                            } else {
                                socket.recv_capacity()
                            }
                        })
                        .unwrap_or(0),
                }
            };
            let mut candidate = if capacity != 0 && size > capacity {
                match crate::net::socket::buffer::try_owned_buffer(
                    &self.memory.sock,
                    &creator,
                    size,
                    0u8,
                ) {
                    Ok(buffer) => Some(smoltcp::socket::tcp::SocketBuffer::from_owned(buffer)),
                    Err(error) => {
                        drop(operation);
                        error.recover(&mut recovery)?;
                        continue;
                    }
                }
            } else {
                None
            };
            let mut retired = None;
            let mut retry = false;
            {
                let mut writer = self.inner.write();
                if let Some(inner) = writer.as_mut() {
                    inner.with_data_socket_mut(|socket| {
                        let capacity = if send {
                            socket.send_capacity()
                        } else {
                            socket.recv_capacity()
                        };
                        if size > capacity {
                            let Some(prepared) = candidate.take() else {
                                retry = true;
                                return;
                            };
                            match if send {
                                socket.replace_prepared_send_buffer(prepared)
                            } else {
                                socket.replace_prepared_receive_buffer(prepared)
                            } {
                                Ok(old) => retired = Some(old),
                                Err(prepared) => {
                                    candidate = Some(prepared);
                                    retry = true;
                                    return;
                                }
                            }
                        }
                        // A shrink retains the real old stock and queued data;
                        // future send/send_slice admission uses the new limit.
                        assert!(if send {
                            socket.set_send_buffer_limit(size)
                        } else {
                            socket.set_receive_buffer_limit(size)
                        });
                    });
                }
                if !retry {
                    let option = if send {
                        &self.options.send_buf_size
                    } else {
                        &self.options.recv_buf_size
                    };
                    option.store(size, core::sync::atomic::Ordering::Relaxed);
                }
            }
            drop(operation);
            drop(candidate);
            drop(retired);
            if !retry {
                return Ok(());
            }
        }
    }

    /// Applies congestion control setting to all applicable inner states.
    pub(crate) fn apply_congestion_control(&self, cc: smoltcp::socket::tcp::CongestionControl) {
        let mut writer = self.inner.write();
        *self.options.congestion_control.lock_irqsave() = cc;
        if let Some(inner) = writer.as_mut() {
            inner.for_each_socket_mut(|s| s.set_congestion_control(cc));
        }
    }

    /// Applies keepalive setting to all applicable inner states.
    pub(crate) fn apply_keepalive(&self, interval: Option<smoltcp::time::Duration>) {
        let mut writer = self.inner.write();
        if let Some(inner) = writer.as_mut() {
            inner.for_each_socket_mut(|s| s.set_keep_alive(interval));
        }
    }

    #[inline]
    pub(super) fn shutdown_bits(&self) -> crate::net::socket::common::ShutdownBit {
        crate::net::socket::common::ShutdownBit::from_bits_truncate(
            self.shutdown.load(core::sync::atomic::Ordering::Relaxed),
        )
    }

    #[inline]
    pub(crate) fn is_recv_shutdown(&self) -> bool {
        self.shutdown_bits()
            .contains(crate::net::socket::common::ShutdownBit::SHUT_RD)
    }

    #[inline]
    pub(crate) fn is_send_shutdown(&self) -> bool {
        self.shutdown_bits()
            .contains(crate::net::socket::common::ShutdownBit::SHUT_WR)
    }

    pub(crate) fn send_timeout(&self) -> Option<crate::time::Duration> {
        let us = self
            .send_timeout_us()
            .load(core::sync::atomic::Ordering::Relaxed);
        if us == u64::MAX {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    pub(crate) fn recv_timeout(&self) -> Option<crate::time::Duration> {
        let us = self
            .recv_timeout_us()
            .load(core::sync::atomic::Ordering::Relaxed);
        if us == u64::MAX {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }
}

impl core::ops::Deref for TcpSocket {
    type Target = TcpSocketData;

    fn deref(&self) -> &TcpSocketData {
        self.data.get()
    }
}
