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
use crate::net::socket::Socket;
use crate::process::namespace::net_namespace::NetNamespace;
use crate::process::ProcessManager;

use super::constants;
use super::inner;
use super::shutdown::ShutdownRecvTracker;

type EP = crate::filesystem::epoll::EPollEventType;

#[derive(Debug)]
pub struct TcpSocketOptions {
    /// SO_SNDTIMEO (microseconds). 0 means "no timeout".
    pub(crate) send_timeout_us: AtomicU64,
    /// SO_RCVTIMEO (microseconds). 0 means "no timeout".
    pub(crate) recv_timeout_us: AtomicU64,

    /// TCP_INQ: whether to report inq bytes via recvmsg cmsg.
    pub(crate) tcp_inq_enabled: AtomicBool,
    /// SO_TIMESTAMP: whether to attach SCM_TIMESTAMP on recvmsg.
    pub(crate) so_timestamp_enabled: AtomicBool,
    /// IP_MTU_DISCOVER: PMTU discovery strategy.
    pub(crate) ip_mtu_discover: core::sync::atomic::AtomicI32,

    pub(crate) send_buf_size: AtomicUsize,
    pub(crate) recv_buf_size: AtomicUsize,

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
            send_timeout_us: AtomicU64::new(0),
            recv_timeout_us: AtomicU64::new(0),

            tcp_inq_enabled: AtomicBool::new(false),
            so_timestamp_enabled: AtomicBool::new(false),
            // Default: IP_PMTUDISC_WANT (1)
            ip_mtu_discover: core::sync::atomic::AtomicI32::new(1),
            send_buf_size: AtomicUsize::new(inner::DEFAULT_TX_BUF_SIZE),
            recv_buf_size: AtomicUsize::new(inner::DEFAULT_RX_BUF_SIZE),

            tcp_max_seg: AtomicUsize::new(constants::DEFAULT_TCP_MSS), // Default MSS
            tcp_defer_accept: AtomicI32::new(0),
            tcp_syncnt: AtomicI32::new(constants::DEFAULT_TCP_SYNCNT), // Default 6
            tcp_window_clamp: AtomicUsize::new(0),
            tcp_user_timeout: AtomicI32::new(0),
            so_filter_attached: AtomicBool::new(false),
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
    pub(crate) inner: RwSem<Option<inner::Inner>>,
    pub(crate) shutdown: AtomicUsize,
    /// If SHUT_WR is requested while `cork_buf` still contains bytes that have not been
    /// handed to the underlying TCP stack, defer sending FIN until those bytes are flushed.
    pub(crate) send_fin_deferred: AtomicBool,
    pub(crate) nonblock: AtomicBool,
    pub(crate) wait_queue: WaitQueue,
    pub(crate) inode_id: InodeId,
    pub(crate) open_files: AtomicUsize,
    pub(crate) self_ref: Weak<Self>,
    pub(crate) pollee: AtomicUsize,
    pub(crate) netns: Arc<NetNamespace>,
    pub(crate) epoll_items: EPollItems,
    pub(crate) fasync_items: FAsyncItems,
    pub(crate) options: TcpSocketOptions,
    pub(crate) cork_buf: Mutex<Vec<u8>>,
    pub(crate) cork_flush_in_progress: AtomicBool,
    pub(crate) cork_timer_active: AtomicBool,
    pub(crate) recv_shutdown: ShutdownRecvTracker,
}

impl TcpSocket {
    fn new_common(
        inner: inner::Inner,
        nonblock: bool,
        netns: Arc<NetNamespace>,
        pollee_bits: usize,
        me: &Weak<Self>,
    ) -> Self {
        Self {
            inner: RwSem::new(Some(inner)),
            shutdown: AtomicUsize::new(0),
            send_fin_deferred: AtomicBool::new(false),
            nonblock: AtomicBool::new(nonblock),
            wait_queue: WaitQueue::default(),
            inode_id: generate_inode_id(),
            open_files: AtomicUsize::new(0),
            self_ref: me.clone(),
            pollee: AtomicUsize::new(pollee_bits),
            netns,
            epoll_items: EPollItems::default(),
            fasync_items: FAsyncItems::default(),
            options: TcpSocketOptions::new(),
            cork_buf: Mutex::new(Vec::new()),
            cork_flush_in_progress: AtomicBool::new(false),
            cork_timer_active: AtomicBool::new(false),
            recv_shutdown: ShutdownRecvTracker::new(),
        }
    }

    pub fn new(nonblock: bool, ver: smoltcp::wire::IpVersion) -> Arc<Self> {
        let netns = ProcessManager::current_netns();
        Arc::new_cyclic(|me| {
            Self::new_common(
                inner::Inner::Init(inner::Init::new(ver)),
                nonblock,
                netns,
                0,
                me,
            )
        })
    }

    pub fn new_established(
        inner: inner::Established,
        nonblock: bool,
        netns: Arc<NetNamespace>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|me| {
            Self::new_common(
                inner::Inner::Established(inner),
                nonblock,
                netns,
                (EP::EPOLLIN.bits() | EP::EPOLLOUT.bits()) as usize,
                me,
            )
        })
    }

    #[inline]
    pub fn is_listening(&self) -> bool {
        matches!(self.inner.read().as_ref(), Some(inner::Inner::Listening(_)))
    }

    #[inline]
    pub(crate) fn inq_enabled(&self) -> bool {
        self.options
            .tcp_inq_enabled
            .load(core::sync::atomic::Ordering::Relaxed)
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
                inner::Inner::SelfConnected(sc) => sc.recv_queue(),
                _ => inner.with_socket(|s| s.recv_queue()),
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

    /// Updates inner socket buffers for Init state.
    pub(crate) fn update_inner_buffers(&self, tx_size: usize, rx_size: usize) {
        let mut writer = self.inner.write();
        match writer.as_mut() {
            Some(inner::Inner::Init(init)) => {
                let _ = init.resize_buffers(rx_size, tx_size);
            }
            Some(inner::Inner::Established(established)) => {
                established.with_mut(|socket| {
                    socket.set_send_buffer_size(tx_size);
                    socket.set_recv_buffer_size(rx_size);
                });
                established.update_io_events(&self.pollee);
            }
            Some(inner::Inner::SelfConnected(sc)) => {
                sc.set_recv_buffer_size(rx_size);
                sc.update_io_events(&self.pollee, self.is_send_shutdown());
            }
            Some(inner::Inner::Connecting(connecting)) => {
                connecting.with_mut(|socket| {
                    socket.set_send_buffer_size(tx_size);
                    socket.set_recv_buffer_size(rx_size);
                });
                let _ = connecting.update_io_events(&self.pollee);
            }
            _ => {}
        }
    }

    /// Applies congestion control setting to all applicable inner states.
    pub(crate) fn apply_congestion_control(&self, cc: smoltcp::socket::tcp::CongestionControl) {
        let mut writer = self.inner.write();
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

    /// Runs a function with mutable access to the Established inner socket.
    /// Returns EINVAL if the socket is not in Established state.
    pub(crate) fn with_inner_established<R>(
        &self,
        f: impl FnOnce(&inner::Established) -> R,
    ) -> Result<R, SystemError> {
        let mut writer = self.inner.write();
        let inner = writer.take().expect("Tcp inner::Inner is None");
        match inner {
            inner::Inner::Established(established) => {
                let result = f(&established);
                writer.replace(inner::Inner::Established(established));
                Ok(result)
            }
            other => {
                writer.replace(other);
                Err(SystemError::EINVAL)
            }
        }
    }

    #[inline]
    fn shutdown_bits(&self) -> crate::net::socket::common::ShutdownBit {
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
        if us == 0 {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    pub(crate) fn recv_timeout(&self) -> Option<crate::time::Duration> {
        let us = self
            .recv_timeout_us()
            .load(core::sync::atomic::Ordering::Relaxed);
        if us == 0 {
            None
        } else {
            Some(crate::time::Duration::from_micros(us))
        }
    }

    pub fn netns(&self) -> Arc<NetNamespace> {
        self.netns.clone()
    }
}
