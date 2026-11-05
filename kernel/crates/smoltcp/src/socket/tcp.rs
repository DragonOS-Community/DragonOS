// Heads up! Before working on this file you should read, at least, RFC 793 and
// the parts of RFC 1122 that discuss TCP, as well as RFC 7323 for some of the TCP options.
// Consult RFC 7414 when implementing a new feature.

#[cfg(all(feature = "alloc", not(target_has_atomic = "ptr")))]
use alloc::rc::Rc as Shared;
#[cfg(all(feature = "alloc", target_has_atomic = "ptr"))]
use alloc::sync::Arc as Shared;
use core::fmt::Display;
#[cfg(feature = "packetmeta-id")]
use core::num::NonZeroU32;
#[cfg(feature = "async")]
use core::task::Waker;
use core::{fmt, mem};

use crate::phy::PacketMeta;
#[cfg(feature = "async")]
use crate::socket::WakerRegistration;
use crate::socket::{Context, PollAt};
use crate::storage::{Assembler, RingBuffer};
use crate::time::{Duration, Instant};
use crate::wire::{
    IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpRepr, IpVersion, TcpControl, TcpRepr,
    TcpSeqNumber, TcpTimestampGenerator, TcpTimestampRepr, TCP_HEADER_LEN,
};

mod congestion;
mod time_wait;
pub(crate) use time_wait::{TimeWaitAction, TimeWaitState};

/// Linux-style active TIME-WAIT reuse selection. Explicitly bound ports and
/// automatically selected ephemeral ports have different safety policies.
#[derive(Debug, Clone, Copy)]
pub enum TimeWaitReuse {
    Explicit,
    Automatic { loopback: bool },
}

/// Optional integration-owned connection identity. Called synchronously while
/// the socket set is locked; implementations must not reenter that socket set.
/// `prepare_open` is the final fallible step of opening a connection.
#[cfg(feature = "alloc")]
pub trait LifecycleObserver: fmt::Debug + Send + Sync {
    fn identity(&self) -> u64;
    fn prepare_open(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        device: u32,
        replacing: Option<u64>,
    ) -> bool;
    fn on_state_change(
        &self,
        state: State,
        local: Option<IpEndpoint>,
        remote: Option<IpEndpoint>,
        device: u32,
    );
}

/// Protocol-independent network failure reported by an ICMP error handler.
/// Integrations translate these reasons to their own socket ABI errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetworkError {
    NetworkUnreachable,
    HostUnreachable,
    ProtocolUnreachable,
    ConnectionRefused,
    PermissionDenied,
    ProtocolError,
    HostDown,
    NoNetwork,
    OperationNotSupported,
    MessageTooLarge,
}

/// A socket error is independent of whether the TCP connection has terminated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorReason {
    Reset,
    Network(NetworkError),
    TimedOut,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PendingError {
    pub reason: ErrorReason,
    /// Transport state before the error, needed by socket compatibility layers.
    pub state: State,
}

macro_rules! tcp_trace {
    ($($arg:expr),*) => (net_log!(trace, $($arg),*));
}

/// Error returned by [`Socket::listen`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ListenError {
    InvalidState,
    Unaddressable,
}

impl Display for ListenError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ListenError::InvalidState => write!(f, "invalid state"),
            ListenError::Unaddressable => write!(f, "unaddressable destination"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ListenError {}

/// Error returned by [`Socket::connect`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum ConnectError {
    InvalidState,
    Unaddressable,
    AddressInUse,
}

impl Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            ConnectError::InvalidState => write!(f, "invalid state"),
            ConnectError::Unaddressable => write!(f, "unaddressable destination"),
            ConnectError::AddressInUse => write!(f, "connection identity already in use"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for ConnectError {}

/// Error returned by [`Socket::send`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SendError {
    InvalidState,
}

impl Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            SendError::InvalidState => write!(f, "invalid state"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SendError {}

/// Error returned by [`Socket::recv`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RecvError {
    InvalidState,
    Finished,
}

impl Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            RecvError::InvalidState => write!(f, "invalid state"),
            RecvError::Finished => write!(f, "operation finished"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RecvError {}

/// Invalid Linux IP{,V6}_MTU_DISCOVER value passed to [`Socket::set_pmtu_discover`].
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct PmtuDiscoverError;

impl Display for PmtuDiscoverError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "invalid PMTU discovery mode")
    }
}

#[cfg(feature = "std")]
impl std::error::Error for PmtuDiscoverError {}

/// A TCP socket ring buffer.
pub type SocketBuffer<'a> = RingBuffer<'a, u8>;

/// The state of a TCP socket, according to [RFC 793].
///
/// [RFC 793]: https://tools.ietf.org/html/rfc793
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum State {
    Closed,
    Listen,
    SynSent,
    SynReceived,
    Established,
    FinWait1,
    FinWait2,
    CloseWait,
    Closing,
    LastAck,
    TimeWait,
}

/// Result of the output integration's complete-packet admission decision.
/// A policy drop differs from a full queue or a missing route: the transport
/// must apply packet-type-specific send/retry semantics.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpPacketAdmission {
    Admitted,
    PolicyDropped,
    /// Output hooks completed, but the final route requires resegmentation.
    MtuRetry(usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TcpDispatchOutcome {
    Done,
    PolicyDeferred,
    MtuRetry,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            State::Closed => write!(f, "CLOSED"),
            State::Listen => write!(f, "LISTEN"),
            State::SynSent => write!(f, "SYN-SENT"),
            State::SynReceived => write!(f, "SYN-RECEIVED"),
            State::Established => write!(f, "ESTABLISHED"),
            State::FinWait1 => write!(f, "FIN-WAIT-1"),
            State::FinWait2 => write!(f, "FIN-WAIT-2"),
            State::CloseWait => write!(f, "CLOSE-WAIT"),
            State::Closing => write!(f, "CLOSING"),
            State::LastAck => write!(f, "LAST-ACK"),
            State::TimeWait => write!(f, "TIME-WAIT"),
        }
    }
}

// Conservative initial RTT estimate.
const RTTE_INITIAL_RTT: u32 = 300;
const RTTE_INITIAL_DEV: u32 = 100;

// Minimum "safety margin" for the RTO that kicks in when the
// variance gets very low.
const RTTE_MIN_MARGIN: u32 = 5;

const RTTE_MIN_RTO: u32 = 10;
const RTTE_MAX_RTO: u32 = 10000;
// An OUTPUT policy denial leaves new data unsent. Linux retries it through
// the persist probe timer, whose floor and ceiling are separate from the
// retransmission estimator used for packets already in flight.
const POLICY_PROBE_MIN_MS: u64 = 200;
const POLICY_PROBE_MAX_MS: u64 = 120_000;

#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct RttEstimator {
    // Using u32 instead of Duration to save space (Duration is i64)
    rtt: u32,
    deviation: u32,
    timestamp: Option<(Instant, TcpSeqNumber)>,
    max_seq_sent: Option<TcpSeqNumber>,
    rto_count: u8,
}

impl Default for RttEstimator {
    fn default() -> Self {
        Self {
            rtt: RTTE_INITIAL_RTT,
            deviation: RTTE_INITIAL_DEV,
            timestamp: None,
            max_seq_sent: None,
            rto_count: 0,
        }
    }
}

impl RttEstimator {
    fn retransmission_timeout(&self) -> Duration {
        let margin = RTTE_MIN_MARGIN.max(self.deviation * 4);
        let ms = (self.rtt + margin).clamp(RTTE_MIN_RTO, RTTE_MAX_RTO);
        Duration::from_millis(ms as u64)
    }

    fn sample(&mut self, new_rtt: u32) {
        // "Congestion Avoidance and Control", Van Jacobson, Michael J. Karels, 1988
        self.rtt = (self.rtt * 7 + new_rtt + 7) / 8;
        let diff = (self.rtt as i32 - new_rtt as i32).unsigned_abs();
        self.deviation = (self.deviation * 3 + diff + 3) / 4;

        self.rto_count = 0;

        let rto = self.retransmission_timeout().total_millis();
        tcp_trace!(
            "rtte: sample={:?} rtt={:?} dev={:?} rto={:?}",
            new_rtt,
            self.rtt,
            self.deviation,
            rto
        );
    }

    fn on_send(&mut self, timestamp: Instant, seq: TcpSeqNumber) {
        if self
            .max_seq_sent
            .map(|max_seq_sent| seq > max_seq_sent)
            .unwrap_or(true)
        {
            self.max_seq_sent = Some(seq);
            if self.timestamp.is_none() {
                self.timestamp = Some((timestamp, seq));
                tcp_trace!("rtte: sampling at seq={:?}", seq);
            }
        }
    }

    fn on_ack(&mut self, timestamp: Instant, seq: TcpSeqNumber) {
        if let Some((sent_timestamp, sent_seq)) = self.timestamp {
            if seq >= sent_seq {
                self.sample((timestamp - sent_timestamp).total_millis() as u32);
                self.timestamp = None;
            }
        }
    }

    fn on_retransmit(&mut self) {
        if self.timestamp.is_some() {
            tcp_trace!("rtte: abort sampling due to retransmit");
        }
        self.timestamp = None;
        self.rto_count = self.rto_count.saturating_add(1);
        if self.rto_count >= 3 {
            // This happens in 2 scenarios:
            // - The RTT is higher than the initial estimate
            // - The network conditions change, suddenly making the RTT much higher
            // In these cases, the estimator can get stuck, because it can't sample because
            // all packets sent would incur a retransmit. To avoid this, force an estimate
            // increase if we see 3 consecutive retransmissions without any successful sample.
            self.rto_count = 0;
            self.rtt = RTTE_MAX_RTO.min(self.rtt * 2);
            let rto = self.retransmission_timeout().total_millis();
            tcp_trace!(
                "rtte: too many retransmissions, increasing: rtt={:?} dev={:?} rto={:?}",
                self.rtt,
                self.deviation,
                rto
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
enum Timer {
    Idle {
        keep_alive_at: Option<Instant>,
    },
    Retransmit {
        expires_at: Instant,
        delay: Duration,
    },
    FastRetransmit,
    Close {
        expires_at: Instant,
    },
}

const ACK_DELAY_DEFAULT: Duration = Duration::from_millis(10);
const CLOSE_DELAY: Duration = Duration::from_millis(10_000);

impl Timer {
    fn new() -> Timer {
        Timer::Idle {
            keep_alive_at: None,
        }
    }

    fn should_keep_alive(&self, timestamp: Instant) -> bool {
        match *self {
            Timer::Idle {
                keep_alive_at: Some(keep_alive_at),
            } if timestamp >= keep_alive_at => true,
            _ => false,
        }
    }

    fn should_retransmit(&self, timestamp: Instant) -> Option<Duration> {
        match *self {
            Timer::Retransmit { expires_at, delay } if timestamp >= expires_at => {
                Some(timestamp - expires_at + delay)
            }
            Timer::FastRetransmit => Some(Duration::from_millis(0)),
            _ => None,
        }
    }

    fn should_close(&self, timestamp: Instant) -> bool {
        match *self {
            Timer::Close { expires_at } if timestamp >= expires_at => true,
            _ => false,
        }
    }

    fn poll_at(&self) -> PollAt {
        match *self {
            Timer::Idle {
                keep_alive_at: Some(keep_alive_at),
            } => PollAt::Time(keep_alive_at),
            Timer::Idle {
                keep_alive_at: None,
            } => PollAt::Ingress,
            Timer::Retransmit { expires_at, .. } => PollAt::Time(expires_at),
            Timer::FastRetransmit => PollAt::Now,
            Timer::Close { expires_at } => PollAt::Time(expires_at),
        }
    }

    fn set_for_idle(&mut self, timestamp: Instant, interval: Option<Duration>) {
        *self = Timer::Idle {
            keep_alive_at: interval.map(|interval| timestamp + interval),
        }
    }

    fn set_keep_alive(&mut self) {
        if let Timer::Idle { keep_alive_at } = self {
            if keep_alive_at.is_none() {
                *keep_alive_at = Some(Instant::from_millis(0))
            }
        }
    }

    fn rewind_keep_alive(&mut self, timestamp: Instant, interval: Option<Duration>) {
        if let Timer::Idle { keep_alive_at } = self {
            *keep_alive_at = interval.map(|interval| timestamp + interval)
        }
    }

    fn set_for_retransmit(&mut self, timestamp: Instant, delay: Duration) {
        match *self {
            Timer::Idle { .. } | Timer::FastRetransmit { .. } => {
                *self = Timer::Retransmit {
                    expires_at: timestamp + delay,
                    delay,
                }
            }
            Timer::Retransmit { expires_at, delay } if timestamp >= expires_at => {
                *self = Timer::Retransmit {
                    expires_at: timestamp + delay,
                    delay: delay * 2,
                }
            }
            Timer::Retransmit { .. } => (),
            Timer::Close { .. } => (),
        }
    }

    fn set_for_fast_retransmit(&mut self) {
        *self = Timer::FastRetransmit
    }

    fn set_for_close(&mut self, timestamp: Instant) {
        *self = Timer::Close {
            expires_at: timestamp + CLOSE_DELAY,
        }
    }

    fn is_retransmit(&self) -> bool {
        match *self {
            Timer::Retransmit { .. } | Timer::FastRetransmit => true,
            _ => false,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum AckDelayTimer {
    Idle,
    Waiting(Instant),
    Immediate,
}

#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub(crate) struct Tuple {
    pub local: IpEndpoint,
    pub remote: IpEndpoint,
}

impl Display for Tuple {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.local, self.remote)
    }
}

/// A congestion control algorithm.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum CongestionControl {
    None,

    #[cfg(feature = "socket-tcp-reno")]
    Reno,

    #[cfg(feature = "socket-tcp-cubic")]
    Cubic,
}

/// State of the most recent BSD/Linux urgent mark. The byte remains in the
/// ordinary sequence space until an ordinary receiver passes the mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UrgentState {
    Pending,
    Valid(u8),
    Read,
}

/// One ordinary receive span. A non-inline urgent byte is represented as a
/// skipped sequence byte, never as a second payload queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReceiveSpan {
    pub offset: usize,
    pub len: usize,
    pub boundary: bool,
}

/// A Transmission Control Protocol socket.
///
/// A TCP socket may passively listen for connections or actively connect to another endpoint.
/// Note that, for listening sockets, there is no "backlog"; to be able to simultaneously
/// accept several connections, as many sockets must be allocated, or any new connection
/// attempts will be reset.
#[derive(Debug)]
pub struct Socket<'a> {
    state: State,
    pending_error: Option<PendingError>,
    soft_error: Option<NetworkError>,
    timer: Timer,
    rtte: RttEstimator,
    assembler: Assembler,
    rx_buffer: SocketBuffer<'a>,
    rx_admission_limit: Option<usize>,
    rx_fin_received: bool,
    /// The local receive half has been shut down by the user.
    ///
    /// This models the BSD/Linux close(2) orphan case. Once the application has
    /// closed the receive side, new data in FIN-WAIT states must reset the
    /// connection instead of being queued indefinitely.
    rx_shutdown: bool,
    rx_urgent: Option<(TcpSeqNumber, UrgentState)>,
    urgent_inline: bool,
    urgent_notification: bool,
    tx_urgent_end: Option<TcpSeqNumber>,
    tx_buffer: SocketBuffer<'a>,
    // Optional integration admission limit, independent of retained physical
    // storage. Shrinking it never discards already queued transmit bytes.
    tx_admission_limit: Option<usize>,
    /// Interval after which, if no inbound packets are received, the connection is aborted.
    timeout: Option<Duration>,
    /// Interval at which keep-alive packets will be sent.
    keep_alive: Option<Duration>,
    /// The time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    hop_limit: Option<u8>,
    /// Address passed to listen(). Listen address is set when listen() is called and
    /// used every time the socket is reset back to the LISTEN state.
    listen_endpoint: IpListenEndpoint,
    /// Optional IP version constraint for incoming connections. Unlike the listen
    /// endpoint, this is configuration and survives resetting the connection.
    listen_ip_version: Option<IpVersion>,
    /// Explicit passive-open policies, selected from the actual admitted SYN
    /// under protocol serialization, never from an external ingress snapshot.
    listen_pmtu_policies: Option<[u8; 2]>,
    recv_error_policies: [bool; 2],
    listen_recv_error_policies: [bool; 2],
    #[cfg(feature = "packetmeta-id")]
    listen_bound_device: Option<NonZeroU32>,
    #[cfg(feature = "packetmeta-id")]
    connection_bound_device: Option<NonZeroU32>,
    /// Current 4-tuple (local and remote endpoints).
    tuple: Option<Tuple>,
    /// The sequence number corresponding to the beginning of the transmit buffer.
    /// I.e. an ACK(local_seq_no+n) packet removes n bytes from the transmit buffer.
    local_seq_no: TcpSeqNumber,
    /// The sequence number corresponding to the beginning of the receive buffer.
    /// I.e. userspace reading n bytes adds n to remote_seq_no.
    remote_seq_no: TcpSeqNumber,
    /// The last sequence number sent.
    /// I.e. in an idle socket, local_seq_no+tx_buffer.len().
    remote_last_seq: TcpSeqNumber,
    /// Highest admitted sequence end; unlike the send cursor this never
    /// rewinds during retransmission. None until a sequence-bearing packet
    /// has actually been admitted by the output integration.
    admitted_seq_end: Option<TcpSeqNumber>,
    /// The last acknowledgement number sent.
    /// I.e. in an idle socket, remote_seq_no+rx_buffer.len().
    remote_last_ack: Option<TcpSeqNumber>,
    /// Last advertised window in actual bytes. SYN windows are unscaled;
    /// ordinary windows include the negotiated scale exactly once.
    remote_last_win: usize,
    /// The sending window scaling factor advertised to remotes which support RFC 1323.
    /// It is zero if the window <= 64KiB and/or the remote does not support it.
    remote_win_shift: u8,
    /// The remote window size, relative to local_seq_no
    /// I.e. we're allowed to send octets until local_seq_no+remote_win_len
    remote_win_len: usize,
    /// The receive window scaling factor for remotes which support RFC 1323, None if unsupported.
    remote_win_scale: Option<u8>,
    /// Whether or not the remote supports selective ACK as described in RFC 2018.
    remote_has_sack: bool,
    /// The maximum number of data octets that the remote side may receive.
    remote_mss: usize,
    /// Last route-selected MTU, shared by dispatch and poll/Nagle decisions.
    output_ip_mtu: Option<usize>,
    pmtu_discover: u8,
    mtu_retry_hint: Option<(u64, usize)>,
    mtu_retry_at: Option<Instant>,
    mtu_retry_failures: u8,
    pmtu_retransmitting: bool,
    /// An unsent data/FIN segment denied by output policy. Unlike a SYN,
    /// this segment has not advanced the send head and needs a retry deadline.
    policy_retry_at: Option<Instant>,
    policy_retry_failures: u8,
    /// The timestamp of the last packet received.
    remote_last_ts: Option<Instant>,
    /// The sequence number of the last packet received, used for sACK
    local_rx_last_seq: Option<TcpSeqNumber>,
    /// The ACK number of the last packet received.
    local_rx_last_ack: Option<TcpSeqNumber>,
    /// The number of packets received directly after
    /// each other which have the same ACK number.
    local_rx_dup_acks: u8,

    /// Duration for Delayed ACK. If None no ACKs will be delayed.
    ack_delay: Option<Duration>,
    /// Delayed ack timer. If set, packets containing exclusively
    /// ACK or window updates (ie, no data) won't be sent until expiry.
    ack_delay_timer: AckDelayTimer,

    /// Used for rate-limiting: No more challenge ACKs will be sent until this instant.
    challenge_ack_timer: Instant,

    /// Nagle's Algorithm enabled.
    nagle: bool,

    /// The congestion control algorithm.
    congestion_controller: congestion::AnyController,

    /// tsval generator - if some, tcp timestamp is enabled
    pub(crate) tsval_generator: Option<TcpTimestampGenerator>,
    /// Administrative capability, independent of the current peer's negotiation.
    configured_tsval_generator: Option<TcpTimestampGenerator>,

    /// 0 if not seen or timestamp not enabled
    last_remote_tsval: u32,
    ts_recent: Option<(u32, Instant)>,
    time_wait_duration: Duration,
    pub(crate) time_wait: Option<TimeWaitState>,
    #[cfg(feature = "alloc")]
    pub(crate) lifecycle_observer: Option<Shared<dyn LifecycleObserver>>,
    #[cfg(feature = "alloc")]
    lifecycle_published: Option<(State, Option<Tuple>, u32)>,

    #[cfg(feature = "async")]
    rx_waker: WakerRegistration,
    #[cfg(feature = "async")]
    tx_waker: WakerRegistration,
}

const DEFAULT_MSS: usize = 536;

impl<'a> Socket<'a> {
    /// Configure the TIME-WAIT protection duration; existing deadlines are not changed.
    pub fn set_time_wait_duration(&mut self, duration: Duration) {
        assert!(duration > Duration::ZERO);
        self.time_wait_duration = duration;
    }

    #[cfg(feature = "alloc")]
    pub fn set_lifecycle_observer(&mut self, observer: Option<Shared<dyn LifecycleObserver>>) {
        self.lifecycle_observer = observer;
        self.lifecycle_published = None;
        self.notify_lifecycle();
    }

    #[cfg(feature = "alloc")]
    pub fn lifecycle_observer(&self) -> Option<&Shared<dyn LifecycleObserver>> {
        self.lifecycle_observer.as_ref()
    }

    pub(crate) fn notify_lifecycle(&mut self) {
        #[cfg(feature = "alloc")]
        if let Some(observer) = &self.lifecycle_observer {
            let state = (self.state, self.tuple, self.lifecycle_device());
            if self.lifecycle_published == Some(state) {
                return;
            }
            observer.on_state_change(
                self.state,
                self.local_endpoint(),
                self.remote_endpoint(),
                self.lifecycle_device(),
            );
            self.lifecycle_published = Some(state);
        }
    }

    pub(crate) fn lifecycle_device(&self) -> u32 {
        #[cfg(feature = "packetmeta-id")]
        {
            self.bound_device().map_or(0, |d| d.get())
        }
        #[cfg(not(feature = "packetmeta-id"))]
        {
            0
        }
    }

    pub(crate) fn prepare_open(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        replacing: Option<u64>,
    ) -> bool {
        self.prepare_open_on_device(local, remote, self.lifecycle_device(), replacing)
    }

    fn prepare_open_on_device(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        device: u32,
        replacing: Option<u64>,
    ) -> bool {
        #[cfg(feature = "alloc")]
        if let Some(observer) = &self.lifecycle_observer {
            return observer.prepare_open(local, remote, device, replacing);
        }
        let _ = (local, remote, device, replacing);
        true
    }

    pub(crate) fn lifecycle_identity(&self) -> Option<u64> {
        #[cfg(feature = "alloc")]
        {
            return self.lifecycle_observer.as_ref().map(|o| o.identity());
        }
        #[cfg(not(feature = "alloc"))]
        {
            None
        }
    }

    pub(crate) fn retire_time_wait(&mut self) {
        self.time_wait = None;
        self.timer = Timer::new();
        self.tuple = None;
        self.set_state(State::Closed);
        // Do not discard unread data belonging to a still-open application FD.
        self.notify_lifecycle();
    }
    #[allow(unused_comparisons)] // small usize platforms always pass rx_capacity check
    /// Create a socket using the given buffers.
    pub fn new<T>(rx_buffer: T, tx_buffer: T) -> Socket<'a>
    where
        T: Into<SocketBuffer<'a>>,
    {
        let (rx_buffer, tx_buffer) = (rx_buffer.into(), tx_buffer.into());
        let rx_capacity = rx_buffer.capacity();

        // From RFC 1323:
        // [...] the above constraints imply that 2 * the max window size must be less
        // than 2**31 [...] Thus, the shift count must be limited to 14 (which allows
        // windows of 2**30 = 1 Gbyte).
        #[cfg(not(target_pointer_width = "16"))] // Prevent overflow
        if rx_capacity > (1 << 30) {
            panic!("receiving buffer too large, cannot exceed 1 GiB")
        }
        let rx_cap_log2 = mem::size_of::<usize>() * 8 - rx_capacity.leading_zeros() as usize;

        Socket {
            state: State::Closed,
            pending_error: None,
            soft_error: None,
            timer: Timer::new(),
            rtte: RttEstimator::default(),
            assembler: Assembler::new(),
            tx_buffer,
            tx_admission_limit: None,
            rx_buffer,
            rx_admission_limit: None,
            rx_fin_received: false,
            rx_shutdown: false,
            rx_urgent: None,
            urgent_inline: false,
            urgent_notification: false,
            tx_urgent_end: None,
            timeout: None,
            keep_alive: None,
            hop_limit: None,
            listen_endpoint: IpListenEndpoint::default(),
            listen_ip_version: None,
            listen_pmtu_policies: None,
            recv_error_policies: [false; 2],
            listen_recv_error_policies: [false; 2],
            #[cfg(feature = "packetmeta-id")]
            listen_bound_device: None,
            #[cfg(feature = "packetmeta-id")]
            connection_bound_device: None,
            tuple: None,
            local_seq_no: TcpSeqNumber::default(),
            remote_seq_no: TcpSeqNumber::default(),
            remote_last_seq: TcpSeqNumber::default(),
            admitted_seq_end: None,
            remote_last_ack: None,
            remote_last_win: 0,
            remote_win_len: 0,
            remote_win_shift: rx_cap_log2.saturating_sub(16) as u8,
            remote_win_scale: None,
            remote_has_sack: false,
            remote_mss: DEFAULT_MSS,
            output_ip_mtu: None,
            pmtu_discover: 1,
            mtu_retry_hint: None,
            mtu_retry_at: None,
            mtu_retry_failures: 0,
            pmtu_retransmitting: false,
            policy_retry_at: None,
            policy_retry_failures: 0,
            remote_last_ts: None,
            local_rx_last_ack: None,
            local_rx_last_seq: None,
            local_rx_dup_acks: 0,
            ack_delay: Some(ACK_DELAY_DEFAULT),
            ack_delay_timer: AckDelayTimer::Idle,
            challenge_ack_timer: Instant::from_secs(0),
            nagle: true,
            tsval_generator: None,
            configured_tsval_generator: None,
            last_remote_tsval: 0,
            ts_recent: None,
            time_wait_duration: CLOSE_DELAY,
            time_wait: None,
            #[cfg(feature = "alloc")]
            lifecycle_observer: None,
            #[cfg(feature = "alloc")]
            lifecycle_published: None,
            congestion_controller: congestion::AnyController::new(),

            #[cfg(feature = "async")]
            rx_waker: WakerRegistration::new(),
            #[cfg(feature = "async")]
            tx_waker: WakerRegistration::new(),
        }
    }

    #[cfg(feature = "alloc")]
    pub fn set_send_buffer_size(
        &mut self,
        size: usize,
    ) -> Result<(), crate::storage::OwnedBufferResizeError> {
        self.tx_buffer.resize(size, 0)
    }

    #[cfg(feature = "alloc")]
    pub fn set_recv_buffer_size(
        &mut self,
        size: usize,
    ) -> Result<(), crate::storage::OwnedBufferResizeError> {
        self.rx_buffer.resize(size, 0)
    }

    /// Atomically install empty, preallocated receive/transmit storage.
    /// Failure returns both candidates unchanged and preserves the socket.
    /// Success returns the old rings for retirement outside integration locks.
    /// RX copies include reassembly bytes beyond the readable queue; neither
    /// sequence state nor the assembler is reset by a capacity update.
    pub fn replace_prepared_buffers(
        &mut self,
        rx: SocketBuffer<'a>,
        tx: SocketBuffer<'a>,
    ) -> Result<(SocketBuffer<'a>, SocketBuffer<'a>), (SocketBuffer<'a>, SocketBuffer<'a>)> {
        let rx_preserve = self.rx_buffer.len()
            + self
                .assembler
                .iter_data(0)
                .map(|(_, end)| end)
                .max()
                .unwrap_or(0);
        let tx_preserve = self.tx_buffer.len();
        if !rx.is_empty()
            || !tx.is_empty()
            || rx.capacity() < rx_preserve
            || rx.capacity() < self.receive_storage_min_capacity()
            || tx.capacity() < tx_preserve
            || rx.capacity() as u64 > (1u64 << 30)
        {
            return Err((rx, tx));
        }
        let old_rx = self
            .rx_buffer
            .replace_prepared(rx, rx_preserve)
            .unwrap_or_else(|_| unreachable!("prevalidated receive storage"));
        let old_tx = self
            .tx_buffer
            .replace_prepared(tx, tx_preserve)
            .unwrap_or_else(|_| unreachable!("prevalidated transmit storage"));
        Ok((old_rx, old_tx))
    }

    /// Install only prepared receive storage, preserving readable bytes and
    /// all out-of-order reassembly bytes. A rejected candidate is unchanged.
    /// The returned old storage must be retired outside integration locks.
    pub fn replace_prepared_receive_buffer(
        &mut self,
        rx: SocketBuffer<'a>,
    ) -> Result<SocketBuffer<'a>, SocketBuffer<'a>> {
        let preserve = self.rx_buffer.len()
            + self
                .assembler
                .iter_data(0)
                .map(|(_, end)| end)
                .max()
                .unwrap_or(0);
        if rx.capacity() as u64 > (1u64 << 30)
            || rx.capacity() < self.receive_storage_min_capacity()
        {
            return Err(rx);
        }
        self.rx_buffer.replace_prepared(rx, preserve)
    }

    /// Install only prepared transmit storage without allocating or retiring
    /// backing storage. Failure preserves both the candidate and socket.
    pub fn replace_prepared_send_buffer(
        &mut self,
        tx: SocketBuffer<'a>,
    ) -> Result<SocketBuffer<'a>, SocketBuffer<'a>> {
        self.tx_buffer.replace_prepared(tx, self.tx_buffer.len())
    }

    /// Restrict future enqueue operations without discarding queued data or
    /// resizing retained physical storage. Return false without mutation when
    /// the requested limit is not supported by the actual backing capacity.
    pub fn set_send_buffer_limit(&mut self, limit: usize) -> bool {
        if limit > self.tx_buffer.capacity() {
            return false;
        }
        self.tx_admission_limit = Some(limit);
        true
    }

    pub fn send_buffer_limit(&self) -> usize {
        self.tx_admission_limit.unwrap_or(self.tx_buffer.capacity())
    }

    pub fn receive_buffer_limit(&self) -> usize {
        self.rx_admission_limit.unwrap_or(self.rx_buffer.capacity())
    }

    fn send_window(&self) -> usize {
        self.tx_buffer
            .window()
            .min(self.tx_admission_limit.map_or(usize::MAX, |limit| {
                limit.saturating_sub(self.tx_buffer.len())
            }))
    }

    /// Enable logical receive admission before the first open, then update
    /// its limit independently of retained backing. Already advertised space
    /// remains valid. First enabling on an existing connection is rejected.
    pub fn set_receive_buffer_limit(&mut self, limit: usize) -> bool {
        if limit > self.rx_buffer.capacity()
            || (self.rx_admission_limit.is_none()
                && (self.state != State::Closed || self.remote_last_ack.is_some()))
        {
            return false;
        }
        self.rx_admission_limit = Some(limit);
        true
    }

    fn receive_slack(&self) -> usize {
        if self.rx_admission_limit.is_some() {
            (1usize << self.remote_win_shift) - 1
        } else {
            0
        }
    }

    fn desired_receive_window(&self) -> usize {
        match self.rx_admission_limit {
            None => self.rx_buffer.window(),
            Some(limit) => limit
                .saturating_sub(self.rx_buffer.len())
                .min(self.rx_buffer.window().saturating_sub(self.receive_slack())),
        }
    }

    fn promised_receive_window(&self) -> usize {
        let Some(ack) = self.remote_last_ack else {
            return 0;
        };
        let edge = ack + self.remote_last_win;
        let next = self.remote_seq_no + self.rx_buffer.len();
        if edge > next {
            edge - next
        } else {
            0
        }
    }

    fn receive_storage_min_capacity(&self) -> usize {
        let Some(ack) = self.remote_last_ack else {
            return 0;
        };
        let edge = ack + self.remote_last_win;
        if edge > self.remote_seq_no {
            (edge - self.remote_seq_no) + self.receive_slack()
        } else {
            0
        }
    }

    fn remember_advertised_window(&mut self, repr: &TcpRepr<'_>) {
        self.remote_last_ack = repr.ack_number;
        self.remote_last_win = Self::advertised_window_bytes(repr, self.remote_win_shift);
    }

    fn advertised_window_bytes(repr: &TcpRepr<'_>, shift: u8) -> usize {
        if repr.control == TcpControl::Syn {
            usize::from(repr.window_len)
        } else {
            usize::from(repr.window_len) << shift
        }
    }

    /// Mutate integration metadata under the caller's exclusive socket guard,
    /// e.g. committing pending -> accepted ownership without moving data.
    #[cfg(feature = "alloc")]
    pub fn owned_buffer_storage_mut(
        &mut self,
    ) -> (
        Option<&mut (dyn crate::storage::OwnedBuffer<u8> + 'a)>,
        Option<&mut (dyn crate::storage::OwnedBuffer<u8> + 'a)>,
    ) {
        (
            self.rx_buffer.owned_storage_mut(),
            self.tx_buffer.owned_storage_mut(),
        )
    }

    /// Enable or disable TCP Timestamp.
    pub fn set_tsval_generator(&mut self, generator: Option<TcpTimestampGenerator>) {
        self.configured_tsval_generator = generator;
        self.tsval_generator = generator;
    }

    /// Return whether TCP Timestamp is enabled.
    pub fn timestamp_enabled(&self) -> bool {
        self.tsval_generator.is_some()
    }

    #[cfg(feature = "alloc")]
    pub(crate) fn timestamp_configured(&self) -> bool {
        self.configured_tsval_generator.is_some()
    }

    /// Set an algorithm for congestion control.
    ///
    /// `CongestionControl::None` indicates that no congestion control is applied.
    /// Options `CongestionControl::Cubic` and `CongestionControl::Reno` are also available.
    /// To use Reno and Cubic, please enable the `socket-tcp-reno` and `socket-tcp-cubic` features
    /// in the `smoltcp` crate, respectively.
    ///
    /// `CongestionControl::Reno` is a classic congestion control algorithm valued for its simplicity.
    /// Despite having a lower algorithmic complexity than `Cubic`,
    /// it is less efficient in terms of bandwidth usage.
    ///
    /// `CongestionControl::Cubic` represents a modern congestion control algorithm designed to
    /// be more efficient and fair compared to `CongestionControl::Reno`.
    /// It is the default choice for Linux, Windows, and macOS.
    /// `CongestionControl::Cubic` relies on double precision (`f64`) floating point operations, which may cause issues in some contexts:
    /// * Small embedded processors (such as Cortex-M0, Cortex-M1, and Cortex-M3) do not have an FPU, and floating point operations consume significant amounts of CPU time and Flash space.
    /// * Interrupt handlers should almost always avoid floating-point operations.
    /// * Kernel-mode code on desktop processors usually avoids FPU operations to reduce the penalty of saving and restoring FPU registers.
    ///
    /// In all these cases, `CongestionControl::Reno` is a better choice of congestion control algorithm.
    pub fn set_congestion_control(&mut self, congestion_control: CongestionControl) {
        use congestion::*;

        self.congestion_controller = match congestion_control {
            CongestionControl::None => AnyController::None(no_control::NoControl),

            #[cfg(feature = "socket-tcp-reno")]
            CongestionControl::Reno => AnyController::Reno(reno::Reno::new()),

            #[cfg(feature = "socket-tcp-cubic")]
            CongestionControl::Cubic => AnyController::Cubic(cubic::Cubic::new()),
        }
    }

    /// Return the current congestion control algorithm.
    pub fn congestion_control(&self) -> CongestionControl {
        use congestion::*;

        match self.congestion_controller {
            AnyController::None(_) => CongestionControl::None,

            #[cfg(feature = "socket-tcp-reno")]
            AnyController::Reno(_) => CongestionControl::Reno,

            #[cfg(feature = "socket-tcp-cubic")]
            AnyController::Cubic(_) => CongestionControl::Cubic,
        }
    }

    /// Register a waker for receive operations.
    ///
    /// The waker is woken on state changes that might affect the return value
    /// of `recv` method calls, such as receiving data, or the socket closing.
    ///
    /// Notes:
    ///
    /// - Only one waker can be registered at a time. If another waker was previously registered,
    ///   it is overwritten and will no longer be woken.
    /// - The Waker is woken only once. Once woken, you must register it again to receive more wakes.
    /// - "Spurious wakes" are allowed: a wake doesn't guarantee the result of `recv` has
    ///   necessarily changed.
    #[cfg(feature = "async")]
    pub fn register_recv_waker(&mut self, waker: &Waker) {
        self.rx_waker.register(waker)
    }

    /// Register a waker for send operations.
    ///
    /// The waker is woken on state changes that might affect the return value
    /// of `send` method calls, such as space becoming available in the transmit
    /// buffer, or the socket closing.
    ///
    /// Notes:
    ///
    /// - Only one waker can be registered at a time. If another waker was previously registered,
    ///   it is overwritten and will no longer be woken.
    /// - The Waker is woken only once. Once woken, you must register it again to receive more wakes.
    /// - "Spurious wakes" are allowed: a wake doesn't guarantee the result of `send` has
    ///   necessarily changed.
    #[cfg(feature = "async")]
    pub fn register_send_waker(&mut self, waker: &Waker) {
        self.tx_waker.register(waker)
    }

    /// Return the timeout duration.
    ///
    /// See also the [set_timeout](#method.set_timeout) method.
    pub fn timeout(&self) -> Option<Duration> {
        self.timeout
    }

    /// Return the ACK delay duration.
    ///
    /// See also the [set_ack_delay](#method.set_ack_delay) method.
    pub fn ack_delay(&self) -> Option<Duration> {
        self.ack_delay
    }

    /// Return whether Nagle's Algorithm is enabled.
    ///
    /// See also the [set_nagle_enabled](#method.set_nagle_enabled) method.
    pub fn nagle_enabled(&self) -> bool {
        self.nagle
    }

    pub fn remote_mss(&self) -> usize {
        self.remote_mss
    }

    pub fn remote_win_len(&self) -> usize {
        self.remote_win_len
    }

    pub fn remote_win_scale(&self) -> Option<u8> {
        self.remote_win_scale
    }

    pub fn rtt(&self) -> u32 {
        self.rtte.rtt
    }

    pub fn rtt_var(&self) -> u32 {
        self.rtte.deviation
    }

    pub fn rto(&self) -> Duration {
        self.rtte.retransmission_timeout()
    }

    pub fn retransmits(&self) -> u8 {
        self.rtte.rto_count
    }

    pub fn cwnd(&self) -> usize {
        self.congestion_controller.cwnd()
    }

    pub fn ssthresh(&self) -> usize {
        self.congestion_controller.ssthresh()
    }

    pub fn local_seq_no(&self) -> TcpSeqNumber {
        self.local_seq_no
    }

    /// Linux IP{,V6}_MTU_DISCOVER values. This controls ICMP learning and
    /// immediate retransmission, not TCP's route-selected segment size.
    pub fn set_pmtu_discover(&mut self, value: u8) -> Result<(), PmtuDiscoverError> {
        if value > 5 {
            return Err(PmtuDiscoverError);
        }
        self.pmtu_discover = value;
        Ok(())
    }

    pub fn pmtu_discover(&self) -> u8 {
        self.pmtu_discover
    }

    /// Configure independent IPv4/IPv6 policies for future passive opens.
    /// Both values are validated before modifying any configuration. Existing
    /// pending/established connections keep their already-selected policy.
    pub fn set_listen_pmtu_policies(&mut self, policies: [u8; 2]) -> Result<(), PmtuDiscoverError> {
        if policies.iter().any(|value| *value > 5) {
            return Err(PmtuDiscoverError);
        }
        self.listen_pmtu_policies = Some(policies);
        Ok(())
    }

    pub fn output_ip_mtu(&self) -> Option<usize> {
        self.output_ip_mtu
    }

    pub fn recv_error_policies(&self) -> [bool; 2] {
        self.recv_error_policies
    }

    pub fn set_recv_error_policy(&mut self, version: crate::wire::IpVersion, enabled: bool) {
        let index = match version {
            #[cfg(feature = "proto-ipv4")]
            crate::wire::IpVersion::Ipv4 => 0,
            #[cfg(feature = "proto-ipv6")]
            crate::wire::IpVersion::Ipv6 => 1,
        };
        self.recv_error_policies[index] = enabled;
    }

    pub fn set_listen_recv_error_policies(&mut self, policies: [bool; 2]) {
        self.listen_recv_error_policies = policies;
    }

    /// Validate the original outbound TCP header quoted by an ICMP error.
    /// The inclusive sequence interval follows Linux's snd_una..snd_nxt
    /// check, using admitted high water rather than the retransmit cursor.
    pub fn accepts_pmtu_quote(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        seq: TcpSeqNumber,
        ingress: u32,
    ) -> bool {
        self.accepts_error_quote(local, remote, seq, ingress)
    }

    /// Associate an ICMP quotation with the currently admitted TCP connection.
    pub fn accepts_error_quote(
        &self,
        local: IpEndpoint,
        remote: IpEndpoint,
        seq: TcpSeqNumber,
        ingress: u32,
    ) -> bool {
        if matches!(self.state, State::Closed | State::Listen | State::TimeWait) {
            return false;
        }
        let Some(tuple) = self.tuple else {
            return false;
        };
        if tuple.local != local || tuple.remote != remote {
            return false;
        }
        // Linux request_sock ICMP association requires exactly the SYNACK ISN,
        // unlike the inclusive send interval used by full TCP sockets.
        if self.is_passive_open() && seq != self.local_seq_no {
            return false;
        }
        #[cfg(feature = "packetmeta-id")]
        if self
            .bound_device()
            .is_some_and(|device| device.get() != ingress)
        {
            return false;
        }
        #[cfg(not(feature = "packetmeta-id"))]
        let _ = ingress;
        self.admitted_seq_end
            .is_some_and(|end| seq >= self.local_seq_no && seq <= end)
    }

    /// Rewind outstanding data after a validated PMTU decrease without
    /// treating the loss as congestion or backing off the RTO estimator.
    /// The existing retransmit timer remains armed, including at zero window.
    pub fn schedule_pmtu_retransmit(&mut self, mtu: usize) -> bool {
        let Some(tuple) = self.tuple else {
            return false;
        };
        if matches!(self.state, State::Closed | State::Listen | State::TimeWait)
            || self.pmtu_discover >= 4
        {
            return false;
        }
        let minimum = match tuple.local.addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(_) => {
                if self.pmtu_discover == 0 {
                    return false;
                }
                crate::wire::IPV4_HEADER_LEN
                    + TCP_HEADER_LEN
                    + if self.tsval_generator.is_some() {
                        12
                    } else {
                        0
                    }
                    + 1
            }
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(_) => 1280,
        };
        if mtu < minimum || !self.output_ip_mtu.is_some_and(|old| mtu < old) {
            return false;
        }
        self.output_ip_mtu = Some(mtu);
        // Cancel Karn-ambiguous sampling only, without RTO count/backoff.
        self.rtte.timestamp = None;
        self.pmtu_retransmitting = self
            .admitted_seq_end
            .is_some_and(|end| end > self.local_seq_no);
        if self.pmtu_retransmitting {
            self.remote_last_seq = self.local_seq_no;
        }
        self.policy_retry_at = None;
        self.policy_retry_failures = 0;
        self.mtu_retry_at = None;
        self.mtu_retry_failures = 0;
        true
    }

    pub fn remote_last_ack(&self) -> Option<TcpSeqNumber> {
        self.remote_last_ack
    }

    /// SND.NXT does not rewind with the RTO/PMTU retransmission cursor.
    fn send_sequence_end(&self) -> TcpSeqNumber {
        self.admitted_seq_end
            .unwrap_or(self.remote_last_seq)
            .max(self.local_seq_no)
    }

    /// Linux tcp_acceptable_seq(): retain SND.NXT unless the peer really
    /// shrank its window, allowing the negotiated scale's rounding loss.
    fn acceptable_ack_sequence(&self) -> TcpSeqNumber {
        let next = self.send_sequence_end();
        let edge = self.local_seq_no + self.remote_win_len;
        if next <= edge
            || (self.remote_win_scale.is_some() && next - edge < (1usize << self.remote_win_shift))
        {
            next
        } else {
            edge
        }
    }

    /// Return the current window field value, including scaling according to RFC 1323.
    ///
    /// Used in internal calculations as well as packet generation.
    #[inline]
    fn scaled_window(&self) -> u16 {
        let mut window = self.desired_receive_window();
        if self.rx_admission_limit.is_some() {
            let scale = 1usize << self.remote_win_shift;
            window = window.max(self.promised_receive_window().div_ceil(scale) * scale);
            // Pre-open slack and prepared storage validation preserve this
            // invariant even when next_ack is not scale-aligned. Clamping then
            // rounding down here would revoke an already advertised edge.
            debug_assert!(window <= self.rx_buffer.window());
        }
        u16::try_from(window >> self.remote_win_shift).unwrap_or(u16::MAX)
    }

    /// Return the last window field value, including scaling according to RFC 1323.
    ///
    /// Used in internal calculations as well as packet generation.
    ///
    /// Unlike `remote_last_win`, we take into account new packets received (but not acknowledged)
    /// since the last window update and adjust the window length accordingly. This ensures a fair
    /// comparison between the last window length and the new window length we're going to
    /// advertise.
    #[inline]
    fn last_scaled_window(&self) -> Option<u16> {
        let last_ack = self.remote_last_ack?;
        let next_ack = self.remote_seq_no + self.rx_buffer.len();

        let last_win = self.remote_last_win;
        let window_edge = last_ack + last_win;
        if next_ack > window_edge {
            return Some(0);
        }
        let last_win_adjusted = window_edge - next_ack;

        Some(u16::try_from(last_win_adjusted >> self.remote_win_shift).unwrap_or(u16::MAX))
    }

    /// Set the timeout duration.
    ///
    /// A socket with a timeout duration set will abort the connection if either of the following
    /// occurs:
    ///
    ///   * After a [connect](#method.connect) call, the remote endpoint does not respond within
    ///     the specified duration;
    ///   * After establishing a connection, there is data in the transmit buffer and the remote
    ///     endpoint exceeds the specified duration between any two packets it sends;
    ///   * After enabling [keep-alive](#method.set_keep_alive), the remote endpoint exceeds
    ///     the specified duration between any two packets it sends.
    pub fn set_timeout(&mut self, duration: Option<Duration>) {
        self.timeout = duration
    }

    /// Set the ACK delay duration.
    ///
    /// By default, the ACK delay is set to 10ms.
    pub fn set_ack_delay(&mut self, duration: Option<Duration>) {
        self.ack_delay = duration
    }

    /// Enable or disable Nagle's Algorithm.
    ///
    /// Also known as "tinygram prevention". By default, it is enabled.
    /// Disabling it is equivalent to Linux's TCP_NODELAY flag.
    ///
    /// When enabled, Nagle's Algorithm prevents sending segments smaller than MSS if
    /// there is data in flight (sent but not acknowledged). In other words, it ensures
    /// at most only one segment smaller than MSS is in flight at a time.
    ///
    /// It ensures better network utilization by preventing sending many very small packets,
    /// at the cost of increased latency in some situations, particularly when the remote peer
    /// has ACK delay enabled.
    pub fn set_nagle_enabled(&mut self, enabled: bool) {
        self.nagle = enabled
    }

    /// Return the keep-alive interval.
    ///
    /// See also the [set_keep_alive](#method.set_keep_alive) method.
    pub fn keep_alive(&self) -> Option<Duration> {
        self.keep_alive
    }

    /// Set the keep-alive interval.
    ///
    /// An idle socket with a keep-alive interval set will transmit a "keep-alive ACK" packet
    /// every time it receives no communication during that interval. As a result, three things
    /// may happen:
    ///
    ///   * The remote endpoint is fine and answers with an ACK packet.
    ///   * The remote endpoint has rebooted and answers with an RST packet.
    ///   * The remote endpoint has crashed and does not answer.
    ///
    /// The keep-alive functionality together with the timeout functionality allows to react
    /// to these error conditions.
    pub fn set_keep_alive(&mut self, interval: Option<Duration>) {
        self.keep_alive = interval;
        if self.keep_alive.is_some() {
            // If the connection is idle and we've just set the option, it would not take effect
            // until the next packet, unless we wind up the timer explicitly.
            self.timer.set_keep_alive();
        }
    }

    /// Return the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// See also the [set_hop_limit](#method.set_hop_limit) method
    pub fn hop_limit(&self) -> Option<u8> {
        self.hop_limit
    }

    /// Set the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// A socket without an explicitly set hop limit value uses the default [IANA recommended]
    /// value (64).
    ///
    /// # Panics
    ///
    /// This function panics if a hop limit value of 0 is given. See [RFC 1122 § 3.2.1.7].
    ///
    /// [IANA recommended]: https://www.iana.org/assignments/ip-parameters/ip-parameters.xhtml
    /// [RFC 1122 § 3.2.1.7]: https://tools.ietf.org/html/rfc1122#section-3.2.1.7
    pub fn set_hop_limit(&mut self, hop_limit: Option<u8>) {
        // A host MUST NOT send a datagram with a hop limit value of 0
        if let Some(0) = hop_limit {
            panic!("the time-to-live value of a packet must not be zero")
        }

        self.hop_limit = hop_limit
    }

    /// Return the listen endpoint
    #[inline]
    pub fn listen_endpoint(&self) -> IpListenEndpoint {
        self.listen_endpoint
    }

    /// Return the IP version constraint for incoming connections.
    pub fn listen_ip_version(&self) -> Option<IpVersion> {
        self.listen_ip_version
    }

    /// Restrict incoming connections to one IP version, or allow either with `None`
    /// (the default). This also applies to concrete listen addresses, which must
    /// match both the address and the constraint.
    ///
    /// The constraint survives closing, aborting and listening again. It does not
    /// affect an existing connection, which is matched using its four-tuple, or
    /// restrict active connections made with [`Self::connect`].
    pub fn set_listen_ip_version(&mut self, version: Option<IpVersion>) {
        self.listen_ip_version = version;
    }

    /// Return the local endpoint, or None if not connected.
    #[inline]
    pub fn local_endpoint(&self) -> Option<IpEndpoint> {
        Some(self.tuple?.local)
    }

    /// Return the remote endpoint, or None if not connected.
    #[inline]
    pub fn remote_endpoint(&self) -> Option<IpEndpoint> {
        Some(self.tuple?.remote)
    }

    /// Return the connection state, in terms of the TCP state machine.
    #[inline]
    pub fn state(&self) -> State {
        self.state
    }

    /// Return the state in which an accepted remote RST closed the connection.
    ///
    /// The event remains available until consumed with [`Self::take_reset`] or
    /// the socket is reused by [`Self::listen`] or [`Self::connect`]. A normal
    /// FIN, a local abort, and a rejected RST do not create an event.
    pub fn reset_state(&self) -> Option<State> {
        self.pending_error
            .filter(|error| error.reason == ErrorReason::Reset)
            .map(|error| error.state)
    }

    /// Consume the pending remote reset event, returning the pre-reset state.
    pub fn take_reset(&mut self) -> Option<State> {
        self.reset_state().map(|state| {
            self.pending_error = None;
            state
        })
    }

    pub fn pending_error(&self) -> Option<PendingError> {
        self.pending_error
    }

    pub fn is_passive_open(&self) -> bool {
        self.state == State::SynReceived && self.listen_endpoint.port != 0
    }

    pub fn take_error(&mut self) -> Option<PendingError> {
        self.pending_error.take()
    }

    /// SO_ERROR may explicitly consume a soft error, unlike ordinary I/O.
    pub fn take_soft_error(&mut self) -> Option<NetworkError> {
        self.soft_error.take()
    }

    /// Restore the next queued ABI error without changing TCP state.
    pub fn set_reported_network_error(&mut self, reason: NetworkError) {
        self.pending_error = Some(PendingError {
            reason: ErrorReason::Network(reason),
            state: self.state,
        });
    }

    /// Apply an already associated and sequence-validated control error.
    /// Passive SYN requests are discarded silently only when their caller's
    /// protocol policy requests it. No synthetic TCP reset is emitted.
    pub fn report_network_error(
        &mut self,
        reason: NetworkError,
        report: bool,
        abort_request: bool,
    ) {
        if matches!(self.state, State::Closed | State::Listen | State::TimeWait) {
            return;
        }
        let state = self.state;
        if state == State::SynReceived && self.listen_endpoint.port != 0 {
            if abort_request {
                self.tuple = None;
                self.ts_recent = None;
                self.last_remote_tsval = 0;
                self.tsval_generator = self.configured_tsval_generator;
                self.remote_last_ack = None;
                self.admitted_seq_end = None;
                self.soft_error = None;
                self.pending_error = None;
                self.set_state(State::Listen);
                self.notify_lifecycle();
            }
            return;
        }
        if matches!(state, State::SynSent | State::SynReceived) {
            self.pending_error = Some(PendingError {
                reason: ErrorReason::Network(reason),
                state,
            });
            self.tx_buffer.clear();
            self.tuple = None;
            self.time_wait = None;
            self.set_state(State::Closed);
            self.notify_lifecycle();
        } else if report {
            self.pending_error = Some(PendingError {
                reason: ErrorReason::Network(reason),
                state,
            });
        } else {
            self.soft_error = Some(reason);
        }
    }

    /// Returns the window scaling factor advertised to the remote.
    /// This is the shift value we use when sending window sizes to the peer.
    #[inline]
    pub fn remote_win_shift(&self) -> u8 {
        self.remote_win_shift
    }

    /// Returns whether the remote endpoint supports Selective ACK (SACK).
    #[inline]
    pub fn remote_has_sack(&self) -> bool {
        self.remote_has_sack
    }

    fn reset(&mut self) {
        let rx_cap_log2 =
            mem::size_of::<usize>() * 8 - self.rx_buffer.capacity().leading_zeros() as usize;

        self.state = State::Closed;
        self.pending_error = None;
        self.soft_error = None;
        self.timer = Timer::new();
        self.rtte = RttEstimator::default();
        self.assembler = Assembler::new();
        self.tx_buffer.clear();
        self.rx_buffer.clear();
        self.rx_fin_received = false;
        self.rx_shutdown = false;
        self.rx_urgent = None;
        self.urgent_notification = false;
        self.tx_urgent_end = None;
        self.time_wait = None;
        self.ts_recent = None;
        self.last_remote_tsval = 0;
        self.tsval_generator = self.configured_tsval_generator;
        self.listen_endpoint = IpListenEndpoint::default();
        self.tuple = None;
        self.local_seq_no = TcpSeqNumber::default();
        self.remote_seq_no = TcpSeqNumber::default();
        self.remote_last_seq = TcpSeqNumber::default();
        self.admitted_seq_end = None;
        self.remote_last_ack = None;
        self.remote_last_win = 0;
        self.remote_win_len = 0;
        self.remote_win_scale = None;
        self.remote_win_shift = rx_cap_log2.saturating_sub(16) as u8;
        self.remote_mss = DEFAULT_MSS;
        self.output_ip_mtu = None;
        self.mtu_retry_hint = None;
        self.mtu_retry_at = None;
        self.mtu_retry_failures = 0;
        self.pmtu_retransmitting = false;
        self.policy_retry_at = None;
        self.policy_retry_failures = 0;
        self.remote_last_ts = None;
        self.ack_delay_timer = AckDelayTimer::Idle;
        self.challenge_ack_timer = Instant::from_secs(0);

        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }
    }

    /// Start listening on the given endpoint.
    ///
    /// This function returns `Err(Error::InvalidState)` if the socket was already open
    /// (see [is_open](#method.is_open)), and `Err(Error::Unaddressable)`
    /// if the port in the given endpoint is zero.
    pub fn listen<T>(&mut self, local_endpoint: T) -> Result<(), ListenError>
    where
        T: Into<IpListenEndpoint>,
    {
        let local_endpoint = local_endpoint.into();
        if local_endpoint.port == 0 {
            return Err(ListenError::Unaddressable);
        }

        if self.is_open() {
            // If we were already listening to same endpoint there is nothing to do; exit early.
            //
            // In the past listening on an socket that was already listening was an error,
            // however this makes writing an acceptor loop with multiple sockets impossible.
            // Without this early exit, if you tried to listen on a socket that's already listening you'll
            // immediately get an error. The only way around this is to abort the socket first
            // before listening again, but this means that incoming connections can actually
            // get aborted between the abort() and the next listen().
            if matches!(self.state, State::Listen) && self.listen_endpoint == local_endpoint {
                return Ok(());
            } else {
                return Err(ListenError::InvalidState);
            }
        }

        self.reset();
        self.listen_endpoint = local_endpoint;
        self.tuple = None;
        self.set_state(State::Listen);
        self.notify_lifecycle();
        Ok(())
    }

    /// Connect to a given endpoint.
    ///
    /// The local port must be provided explicitly. Assuming `fn get_ephemeral_port() -> u16`
    /// allocates a port between 49152 and 65535, a connection may be established as follows:
    ///
    /// ```no_run
    /// # #[cfg(all(
    /// #     feature = "medium-ethernet",
    /// #     feature = "proto-ipv4",
    /// # ))]
    /// # {
    /// # use smoltcp::socket::tcp::{Socket, SocketBuffer};
    /// # use smoltcp::iface::Interface;
    /// # use smoltcp::wire::IpAddress;
    /// #
    /// # fn get_ephemeral_port() -> u16 {
    /// #     49152
    /// # }
    /// #
    /// # let mut socket = Socket::new(
    /// #     SocketBuffer::new(vec![0; 1200]),
    /// #     SocketBuffer::new(vec![0; 1200])
    /// # );
    /// #
    /// # let mut iface: Interface = todo!();
    /// #
    /// socket.connect(
    ///     iface.context(),
    ///     (IpAddress::v4(10, 0, 0, 1), 80),
    ///     get_ephemeral_port()
    /// ).unwrap();
    /// # }
    /// ```
    ///
    /// The local address may optionally be provided.
    ///
    /// This function returns an error if the socket was open; see [is_open](#method.is_open).
    /// It also returns an error if the local or remote port is zero, or if the remote address
    /// is unspecified.
    pub fn connect<T, U>(
        &mut self,
        cx: &mut Context,
        remote_endpoint: T,
        local_endpoint: U,
    ) -> Result<(), ConnectError>
    where
        T: Into<IpEndpoint>,
        U: Into<IpListenEndpoint>,
    {
        let remote_endpoint: IpEndpoint = remote_endpoint.into();
        let local_endpoint: IpListenEndpoint = local_endpoint.into();

        if self.is_open() {
            return Err(ConnectError::InvalidState);
        }
        if remote_endpoint.port == 0 || remote_endpoint.addr.is_unspecified() {
            return Err(ConnectError::Unaddressable);
        }
        if local_endpoint.port == 0 {
            return Err(ConnectError::Unaddressable);
        }

        // If local address is not provided, choose it automatically.
        let local_endpoint = IpEndpoint {
            addr: match local_endpoint.addr {
                Some(addr) => {
                    if addr.is_unspecified() {
                        return Err(ConnectError::Unaddressable);
                    }
                    addr
                }
                None => cx
                    .get_source_address(&remote_endpoint.addr)
                    .ok_or(ConnectError::Unaddressable)?,
            },
            port: local_endpoint.port,
        };

        if local_endpoint.addr.version() != remote_endpoint.addr.version() {
            return Err(ConnectError::Unaddressable);
        }

        if !self.prepare_open(local_endpoint, remote_endpoint, None) {
            return Err(ConnectError::AddressInUse);
        }
        self.open_connection(cx, local_endpoint, remote_endpoint, None);
        Ok(())
    }

    pub(crate) fn open_connection(
        &mut self,
        cx: &mut Context,
        local_endpoint: IpEndpoint,
        remote_endpoint: IpEndpoint,
        reuse: Option<&TimeWaitState>,
    ) {
        self.reset();
        self.tuple = Some(Tuple {
            local: local_endpoint,
            remote: remote_endpoint,
        });
        self.set_state(State::SynSent);

        let seq = reuse.map_or_else(|| Self::random_seq_no(cx), TimeWaitState::new_isn);
        self.local_seq_no = seq;
        self.remote_last_seq = seq;
        self.admitted_seq_end = None;
        if let Some(old) = reuse {
            self.ts_recent = old.ts_recent;
        }
        self.notify_lifecycle();
    }

    #[cfg(test)]
    fn random_seq_no(_cx: &mut Context) -> TcpSeqNumber {
        TcpSeqNumber(10000)
    }

    #[cfg(not(test))]
    fn random_seq_no(cx: &mut Context) -> TcpSeqNumber {
        TcpSeqNumber(cx.rand().rand_u32() as i32)
    }

    /// Close the transmit half of the full-duplex connection.
    ///
    /// Note that there is no corresponding function for the receive half of the full-duplex
    /// connection; only the remote end can close it. If you no longer wish to receive any
    /// data and would like to reuse the socket right away, use [abort](#method.abort).
    pub fn close(&mut self) {
        match self.state {
            // In the LISTEN state there is no established connection.
            State::Listen => self.set_state(State::Closed),
            // In the SYN-SENT state the remote endpoint is not yet synchronized and, upon
            // receiving an RST, will abort the connection.
            State::SynSent => self.set_state(State::Closed),
            // In the SYN-RECEIVED, ESTABLISHED and CLOSE-WAIT states the transmit half
            // of the connection is open, and needs to be explicitly closed with a FIN.
            State::SynReceived | State::Established => self.set_state(State::FinWait1),
            State::CloseWait => self.set_state(State::LastAck),
            // In the FIN-WAIT-1, FIN-WAIT-2, CLOSING, LAST-ACK, TIME-WAIT and CLOSED states,
            // the transmit half of the connection is already closed, and no further
            // action is needed.
            State::FinWait1
            | State::FinWait2
            | State::Closing
            | State::TimeWait
            | State::LastAck
            | State::Closed => (),
        }
        self.notify_lifecycle();
    }

    /// Aborts the connection, if any.
    ///
    /// This function instantly closes the socket. One reset packet will be sent to the remote
    /// endpoint.
    ///
    /// In terms of the TCP state machine, the socket may be in any state and is moved to
    /// the `CLOSED` state.
    pub fn abort(&mut self) {
        self.set_state(State::Closed);
        self.time_wait = None;
        self.notify_lifecycle();
    }

    /// Shut down the receive half of the connection.
    ///
    /// This mirrors the transport effect of Linux `sk_shutdown & RCV_SHUTDOWN`.
    /// In FIN-WAIT states, any new data that advances the peer sequence space
    /// actively resets the connection instead of being queued.
    #[inline]
    pub fn shutdown_recv(&mut self) {
        self.rx_shutdown = true;
    }

    /// Return whether the receive half has been explicitly shut down.
    #[inline]
    pub fn recv_shutdown(&self) -> bool {
        self.rx_shutdown
    }

    /// Return whether the socket is passively listening for incoming connections.
    ///
    /// In terms of the TCP state machine, the socket must be in the `LISTEN` state.
    #[inline]
    pub fn is_listening(&self) -> bool {
        match self.state {
            State::Listen => true,
            _ => false,
        }
    }

    /// Return whether the socket is open.
    ///
    /// This function returns true if the socket will process incoming or dispatch outgoing
    /// packets. Note that this does not mean that it is possible to send or receive data through
    /// the socket; for that, use [can_send](#method.can_send) or [can_recv](#method.can_recv).
    ///
    /// In terms of the TCP state machine, the socket must not be in the `CLOSED`
    /// or `TIME-WAIT` states.
    #[inline]
    pub fn is_open(&self) -> bool {
        match self.state {
            State::Closed => false,
            State::TimeWait => false,
            _ => true,
        }
    }

    /// Return whether a connection is active.
    ///
    /// This function returns true if the socket is actively exchanging packets with
    /// a remote endpoint. Note that this does not mean that it is possible to send or receive
    /// data through the socket; for that, use [can_send](#method.can_send) or
    /// [can_recv](#method.can_recv).
    ///
    /// If a connection is established, [abort](#method.close) will send a reset to
    /// the remote endpoint.
    ///
    /// In terms of the TCP state machine, the socket must not be in the `CLOSED`, `TIME-WAIT`,
    /// or `LISTEN` state.
    #[inline]
    pub fn is_active(&self) -> bool {
        match self.state {
            State::Closed => false,
            State::TimeWait => false,
            State::Listen => false,
            _ => true,
        }
    }

    /// Return whether the transmit half of the full-duplex connection is open.
    ///
    /// This function returns true if it's possible to send data and have it arrive
    /// to the remote endpoint. However, it does not make any guarantees about the state
    /// of the transmit buffer, and even if it returns true, [send](#method.send) may
    /// not be able to enqueue any octets.
    ///
    /// In terms of the TCP state machine, the socket must be in the `ESTABLISHED` or
    /// `CLOSE-WAIT` state.
    #[inline]
    pub fn may_send(&self) -> bool {
        match self.state {
            State::Established => true,
            // In CLOSE-WAIT, the remote endpoint has closed our receive half of the connection
            // but we still can transmit indefinitely.
            State::CloseWait => true,
            _ => false,
        }
    }

    /// Return whether the receive half of the full-duplex connection is open.
    ///
    /// This function returns true if it's possible to receive data from the remote endpoint.
    /// It will return true while there is data in the receive buffer, and if there isn't,
    /// as long as the remote endpoint has not closed the connection.
    ///
    /// In terms of the TCP state machine, the socket must be in the `ESTABLISHED`,
    /// `FIN-WAIT-1`, or `FIN-WAIT-2` state, or have data in the receive buffer instead.
    #[inline]
    pub fn may_recv(&self) -> bool {
        if self.rx_shutdown && self.rx_buffer.is_empty() {
            return false;
        }

        match self.state {
            State::Established => true,
            // In FIN-WAIT-1/2, we have closed our transmit half of the connection but
            // we still can receive indefinitely.
            State::FinWait1 | State::FinWait2 => true,
            // If we have something in the receive buffer, we can receive that.
            _ if !self.rx_buffer.is_empty() => true,
            _ => false,
        }
    }

    /// Check whether the transmit half of the full-duplex connection is open
    /// (see [may_send](#method.may_send)), and the transmit buffer is not full.
    #[inline]
    pub fn can_send(&self) -> bool {
        if !self.may_send() {
            return false;
        }

        self.send_window() != 0
    }

    /// Return the maximum number of bytes inside the recv buffer.
    #[inline]
    pub fn recv_capacity(&self) -> usize {
        self.rx_buffer.capacity()
    }

    /// Return the maximum number of bytes inside the transmit buffer.
    #[inline]
    pub fn send_capacity(&self) -> usize {
        self.tx_buffer.capacity()
    }

    /// Check whether the receive half of the full-duplex connection buffer is open
    /// (see [may_recv](#method.may_recv)), and the receive buffer is not empty.
    #[inline]
    pub fn can_recv(&self) -> bool {
        if !self.may_recv() {
            return false;
        }

        !self.rx_buffer.is_empty()
    }

    fn send_impl<'b, F, R>(&'b mut self, f: F) -> Result<R, SendError>
    where
        F: FnOnce(&'b mut SocketBuffer<'a>, usize) -> (usize, R),
    {
        if !self.may_send() {
            return Err(SendError::InvalidState);
        }

        let old_length = self.tx_buffer.len();
        let window = self.send_window();
        let (size, result) = f(&mut self.tx_buffer, window);
        if size > 0 {
            // The connection might have been idle for a long time, and so remote_last_ts
            // would be far in the past. Unless we clear it here, we'll abort the connection
            // down over in dispatch() by erroneously detecting it as timed out.
            if old_length == 0 {
                self.remote_last_ts = None
            }

            #[cfg(any(test, feature = "verbose"))]
            tcp_trace!(
                "tx buffer: enqueueing {} octets (now {})",
                size,
                old_length + size
            );
        }
        Ok(result)
    }

    /// Call `f` with the largest contiguous slice of octets in the transmit buffer,
    /// and enqueue the amount of elements returned by `f`.
    ///
    /// This function returns `Err(Error::Illegal)` if the transmit half of
    /// the connection is not open; see [may_send](#method.may_send).
    pub fn send<'b, F, R>(&'b mut self, f: F) -> Result<R, SendError>
    where
        F: FnOnce(&'b mut [u8]) -> (usize, R),
    {
        self.send_impl(|tx_buffer, window| {
            tx_buffer.enqueue_many_with(|slice| {
                let available = slice.len().min(window);
                let (size, result) = f(&mut slice[..available]);
                assert!(
                    size <= available,
                    "enqueue count exceeds offered send slice"
                );
                (size, result)
            })
        })
    }

    /// Enqueue a sequence of octets to be sent, and fill it from a slice.
    ///
    /// This function returns the amount of octets actually enqueued, which is limited
    /// by the amount of free space in the transmit buffer; down to zero.
    ///
    /// See also [send](#method.send).
    pub fn send_slice(&mut self, data: &[u8]) -> Result<usize, SendError> {
        self.send_impl(|tx_buffer, window| {
            let size = tx_buffer.enqueue_slice(&data[..data.len().min(window)]);
            (size, size)
        })
    }

    fn recv_error_check(&mut self) -> Result<(), RecvError> {
        // We may have received some data inside the initial SYN, but until the connection
        // is fully open we must not dequeue any data, as it may be overwritten by e.g.
        // another (stale) SYN. (We do not support TCP Fast Open.)
        if !self.may_recv() {
            if self.rx_fin_received {
                return Err(RecvError::Finished);
            }
            return Err(RecvError::InvalidState);
        }

        Ok(())
    }

    fn recv_impl<'b, F, R>(&'b mut self, f: F) -> Result<R, RecvError>
    where
        F: FnOnce(&'b mut SocketBuffer<'a>) -> (usize, R),
    {
        self.recv_error_check()?;

        let _old_length = self.rx_buffer.len();
        let (size, result) = f(&mut self.rx_buffer);
        self.remote_seq_no += size;
        if self
            .rx_urgent
            .is_some_and(|(mark, _)| self.remote_seq_no - usize::from(self.rx_fin_received) > mark)
        {
            self.rx_urgent = None;
        }
        if size > 0 {
            #[cfg(any(test, feature = "verbose"))]
            tcp_trace!(
                "rx buffer: dequeueing {} octets (now {})",
                size,
                _old_length - size
            );
        }
        Ok(result)
    }

    /// Call `f` with the largest contiguous slice of octets in the receive buffer,
    /// and dequeue the amount of elements returned by `f`.
    ///
    /// This function errors if the receive half of the connection is not open.
    ///
    /// If the receive half has been gracefully closed (with a FIN packet), `Err(Error::Finished)`
    /// is returned. In this case, the previously received data is guaranteed to be complete.
    ///
    /// In all other cases, `Err(Error::Illegal)` is returned and previously received data (if any)
    /// may be incomplete (truncated).
    pub fn recv<'b, F, R>(&'b mut self, f: F) -> Result<R, RecvError>
    where
        F: FnOnce(&'b mut [u8]) -> (usize, R),
    {
        self.recv_impl(|rx_buffer| rx_buffer.dequeue_many_with(f))
    }

    /// Dequeue a sequence of received octets, and fill a slice from it.
    ///
    /// This function returns the amount of octets actually dequeued, which is limited
    /// by the amount of occupied space in the receive buffer; down to zero.
    ///
    /// See also [recv](#method.recv).
    pub fn recv_slice(&mut self, data: &mut [u8]) -> Result<usize, RecvError> {
        self.recv_impl(|rx_buffer| {
            let size = rx_buffer.dequeue_slice(data);
            (size, size)
        })
    }

    /// Peek at a sequence of received octets without removing them from
    /// the receive buffer, and return a pointer to it.
    ///
    /// This function otherwise behaves identically to [recv](#method.recv).
    pub fn peek(&mut self, size: usize) -> Result<&[u8], RecvError> {
        self.recv_error_check()?;

        let buffer = self.rx_buffer.get_allocated(0, size);
        if !buffer.is_empty() {
            #[cfg(any(test, feature = "verbose"))]
            tcp_trace!("rx buffer: peeking at {} octets", buffer.len());
        }
        Ok(buffer)
    }

    /// Peek at a sequence of received octets without removing them from
    /// the receive buffer, and fill a slice from it.
    ///
    /// This function otherwise behaves identically to [recv_slice](#method.recv_slice).
    pub fn peek_slice(&mut self, data: &mut [u8]) -> Result<usize, RecvError> {
        self.peek_slice_at(0, data)
    }

    /// Peek at received octets starting at `offset` past the unread head,
    /// without consuming data or pending errors.
    ///
    /// Returns zero when the offset is at or beyond the queued data. Like
    /// [`Self::peek_slice`], this copies across a receive-buffer wrap.
    pub fn peek_slice_at(&mut self, offset: usize, data: &mut [u8]) -> Result<usize, RecvError> {
        // Guard before ring indexing, including for an arbitrarily large offset.
        if offset >= self.rx_buffer.len() {
            return Ok(0);
        }
        Ok(self.rx_buffer.read_allocated(offset, data))
    }

    /// Select a bounded ordinary span without changing the receive head.
    /// Callers retain the returned physical offset when peeking across a hole.
    pub fn receive_span(&self, offset: usize, limit: usize) -> ReceiveSpan {
        let queued = self.rx_buffer.len();
        let mut offset = offset.min(queued);
        let mut boundary = false;
        let mut end = queued;
        if let Some((mark, _)) = self.rx_urgent {
            let seq = self.recv_head_seq() + offset;
            if seq < mark {
                end = end.min(offset + (mark - seq));
                boundary = end == offset + (mark - seq);
            } else if seq == mark && !self.urgent_inline && offset < queued {
                offset += 1;
            }
        }
        let len = limit.min(end.saturating_sub(offset));
        ReceiveSpan {
            offset,
            len,
            boundary: boundary && offset + len == end,
        }
    }

    /// FIONREAD stops at the urgent mark unless SO_OOBINLINE is enabled.
    pub fn ordinary_queue(&self) -> usize {
        if !self.urgent_inline {
            if let Some((mark, _)) = self.rx_urgent {
                let head = self.recv_head_seq();
                if mark >= head && mark < head + self.rx_buffer.len() {
                    return mark - head;
                }
            }
        }
        self.rx_buffer.len()
    }

    pub fn ordinary_can_recv(&self) -> bool {
        self.receive_span(0, usize::MAX).len != 0
    }

    /// Whether FIN has been accepted, independently of queued payload.
    pub fn received_fin(&self) -> bool {
        self.rx_fin_received
    }

    pub fn urgent_state(&self) -> Option<UrgentState> {
        self.rx_urgent.map(|(_, state)| state)
    }

    pub fn at_urgent_mark(&self) -> bool {
        self.rx_urgent
            .is_some_and(|(mark, _)| mark == self.recv_head_seq())
    }

    pub fn urgent_inline(&self) -> bool {
        self.urgent_inline
    }

    pub fn set_urgent_inline(&mut self, enabled: bool) {
        self.urgent_inline = enabled;
    }

    /// OOB consumption is independent of ordinary stream consumption.
    pub fn consume_urgent(&mut self) {
        if let Some((_, state)) = self.rx_urgent.as_mut() {
            *state = UrgentState::Read;
        }
    }

    pub fn take_urgent_notification(&mut self) -> bool {
        core::mem::take(&mut self.urgent_notification)
    }

    /// Queue data and publish the accepted urgent prefix in one transport
    /// transaction. `urgent_end` is a one-based offset in `data`; a partial
    /// enqueue advances the mark as Linux does before waiting for send space.
    pub fn send_slice_with_urgent(
        &mut self,
        data: &[u8],
        urgent_end: Option<usize>,
    ) -> Result<usize, SendError> {
        let start = self.local_seq_no + self.tx_buffer.len();
        let written = self.send_slice(data)?;
        if let Some(end) = urgent_end.filter(|end| *end != 0) {
            if written != 0 {
                self.tx_urgent_end = Some(start + end.min(written));
            }
        }
        Ok(written)
    }

    fn process_urgent(&mut self, repr: &TcpRepr) {
        if let Some(pointer) = repr.urgent_at {
            let mark = repr.seq_number + usize::from(pointer.saturating_sub(1));
            let head = self.recv_head_seq();
            let next = head + self.rx_buffer.len();
            if mark >= head && mark >= next && self.rx_urgent.map_or(true, |(old, _)| mark > old) {
                // Replacing a mark at the current head must not expose its
                // already excluded byte as ordinary data.
                if self.at_urgent_mark() && !self.urgent_inline && !self.rx_buffer.is_empty() {
                    let _ = self.rx_buffer.dequeue_many(1);
                    self.remote_seq_no += 1;
                }
                self.rx_urgent = Some((mark, UrgentState::Pending));
                self.urgent_notification = true;
            }
        }
        if let Some((mark, state @ UrgentState::Pending)) = self.rx_urgent.as_mut() {
            let start = repr.seq_number + usize::from(repr.control == TcpControl::Syn);
            if *mark >= start && *mark < start + repr.payload.len() {
                *state = UrgentState::Valid(repr.payload[*mark - start]);
            }
        }
    }

    /// Return the sequence position of the first unread payload byte.
    ///
    /// Consuming payload advances this position; receiving more payload or a
    /// FIN does not. A caller keeping a private peek cursor can compare this
    /// position between receive attempts to detect another consumer.
    pub fn recv_head_seq(&self) -> TcpSeqNumber {
        // remote_seq_no includes an accepted FIN before queued payload has
        // necessarily been consumed. Exclude that control byte from the head.
        self.remote_seq_no - usize::from(self.rx_fin_received)
    }

    /// Return the amount of octets queued in the transmit buffer.
    ///
    /// Note that the Berkeley sockets interface does not have an equivalent of this API.
    pub fn send_queue(&self) -> usize {
        self.tx_buffer.len()
    }

    /// Return the amount of octets queued in the receive buffer. This value can be larger than
    /// the slice read by the next `recv` or `peek` call because it includes all queued octets,
    /// and not only the octets that may be returned as a contiguous slice.
    ///
    /// Note that the Berkeley sockets interface does not have an equivalent of this API.
    pub fn recv_queue(&self) -> usize {
        self.rx_buffer.len()
    }

    fn set_state(&mut self, state: State) {
        if self.state != state {
            tcp_trace!("state={}=>{}", self.state, state);
        }

        self.state = state;

        #[cfg(feature = "async")]
        {
            // Wake all tasks waiting. Even if we haven't received/sent data, this
            // is needed because return values of functions may change depending on the state.
            // For example, a pending read has to fail with an error if the socket is closed.
            self.rx_waker.wake();
            self.tx_waker.wake();
        }
    }

    pub fn reply(ip_repr: &IpRepr, repr: &TcpRepr) -> (IpRepr, TcpRepr<'static>) {
        let reply_repr = TcpRepr {
            src_port: repr.dst_port,
            dst_port: repr.src_port,
            control: TcpControl::None,
            seq_number: TcpSeqNumber(0),
            ack_number: None,
            urgent_at: None,
            window_len: 0,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: None,
            payload: &[],
        };
        let ip_reply_repr = IpRepr::new(
            ip_repr.dst_addr(),
            ip_repr.src_addr(),
            IpProtocol::Tcp,
            reply_repr.buffer_len(),
            64,
        );
        (ip_reply_repr, reply_repr)
    }

    pub fn rst_reply(ip_repr: &IpRepr, repr: &TcpRepr) -> (IpRepr, TcpRepr<'static>) {
        debug_assert!(repr.control != TcpControl::Rst);

        let (ip_reply_repr, mut reply_repr) = Self::reply(ip_repr, repr);

        // See https://www.snellman.net/blog/archive/2016-02-01-tcp-rst/ for explanation
        // of why we sometimes send an RST and sometimes an RST|ACK
        reply_repr.control = TcpControl::Rst;
        reply_repr.seq_number = repr.ack_number.unwrap_or_default();
        if repr.control == TcpControl::Syn && repr.ack_number.is_none() {
            reply_repr.ack_number = Some(repr.seq_number + repr.segment_len());
        }

        (ip_reply_repr, reply_repr)
    }

    fn ack_reply(&mut self, ip_repr: &IpRepr, repr: &TcpRepr) -> (IpRepr, TcpRepr<'static>) {
        let (mut ip_reply_repr, mut reply_repr) = Self::reply(ip_repr, repr);
        reply_repr.timestamp = repr
            .timestamp
            .and_then(|tcp_ts| tcp_ts.generate_reply(self.tsval_generator));

        // From RFC 793:
        // [...] an empty acknowledgment segment containing the current send-sequence number
        // and an acknowledgment indicating the next sequence number expected
        // to be received.
        reply_repr.seq_number = self.acceptable_ack_sequence();
        reply_repr.ack_number = Some(self.remote_seq_no + self.rx_buffer.len());

        // From RFC 1323:
        // The window field [...] of every outgoing segment, with the exception of SYN
        // segments, is right-shifted by [advertised scale value] bits[...]
        reply_repr.window_len = self.scaled_window();
        self.remember_advertised_window(&reply_repr);

        // If the remote supports selective acknowledgement, add the option to the outgoing
        // segment.
        if self.remote_has_sack {
            net_debug!("sending sACK option with current assembler ranges");

            // RFC 2018: The first SACK block (i.e., the one immediately following the kind and
            // length fields in the option) MUST specify the contiguous block of data containing
            // the segment which triggered this ACK, unless that segment advanced the
            // Acknowledgment Number field in the header.
            reply_repr.sack_ranges[0] = None;

            if let Some(last_seg_seq) = self.local_rx_last_seq.map(|s| s.0 as u32) {
                reply_repr.sack_ranges[0] = self
                    .assembler
                    .iter_data(reply_repr.ack_number.map(|s| s.0 as usize).unwrap_or(0))
                    .map(|(left, right)| (left as u32, right as u32))
                    .find(|(left, right)| *left <= last_seg_seq && *right >= last_seg_seq);
            }

            if reply_repr.sack_ranges[0].is_none() {
                // The matching segment was removed from the assembler, meaning the acknowledgement
                // number has advanced, or there was no previous sACK.
                //
                // While the RFC says we SHOULD keep a list of reported sACK ranges, and iterate
                // through those, that is currently infeasible. Instead, we offer the range with
                // the lowest sequence number (if one exists) to hint at what segments would
                // most quickly advance the acknowledgement number.
                reply_repr.sack_ranges[0] = self
                    .assembler
                    .iter_data(reply_repr.ack_number.map(|s| s.0 as usize).unwrap_or(0))
                    .map(|(left, right)| (left as u32, right as u32))
                    .next();
            }
        }

        // Since the sACK option may have changed the length of the payload, update that.
        ip_reply_repr.set_payload_len(reply_repr.buffer_len());
        (ip_reply_repr, reply_repr)
    }

    fn challenge_ack_reply(
        &mut self,
        cx: &mut Context,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        if cx.now() < self.challenge_ack_timer {
            return None;
        }

        // Rate-limit to 1 per second max.
        self.challenge_ack_timer = cx.now() + Duration::from_secs(1);

        Some(self.ack_reply(ip_repr, repr))
    }

    #[inline]
    fn should_reset_receive_shutdown_data(
        &self,
        segment_start: TcpSeqNumber,
        segment_end: TcpSeqNumber,
        window_start: TcpSeqNumber,
    ) -> bool {
        self.rx_shutdown
            && matches!(self.state, State::FinWait1 | State::FinWait2)
            && segment_start != segment_end
            && segment_end > window_start
    }

    /// Set the device constraint for future passive connections.
    ///
    /// Existing handshakes and connections retain their device. A handshake
    /// reset back to LISTEN uses the latest setting. Device IDs are supplied by
    /// the embedding application's RX packet metadata; `None` accepts any device.
    #[cfg(feature = "packetmeta-id")]
    pub fn set_listen_bound_device(&mut self, device: Option<NonZeroU32>) {
        self.listen_bound_device = device;
    }

    /// Set the device constraint for this connection and future listening.
    /// The constraint survives connection resets. Outgoing packet metadata uses
    /// this device ID, or zero when unconstrained.
    #[cfg(feature = "packetmeta-id")]
    pub fn set_bound_device(&mut self, device: Option<NonZeroU32>) {
        self.listen_bound_device = device;
        self.connection_bound_device = device;
    }

    /// Return the device constraint currently used for input and output.
    #[cfg(feature = "packetmeta-id")]
    pub fn bound_device(&self) -> Option<NonZeroU32> {
        if self.state == State::Listen {
            self.listen_bound_device
        } else {
            self.connection_bound_device
        }
    }

    pub(crate) fn accepts_ingress(&self, _meta: PacketMeta) -> bool {
        #[cfg(feature = "packetmeta-id")]
        if let Some(device) = self.bound_device() {
            return device.get() == _meta.id;
        }
        true
    }

    pub(crate) fn egress_meta(&self) -> PacketMeta {
        #[allow(unused_mut)]
        let mut meta = PacketMeta::default();
        #[cfg(feature = "packetmeta-id")]
        {
            meta.id = self.bound_device().map_or(0, NonZeroU32::get);
        }
        meta
    }

    pub fn accepts(&self, _cx: &mut Context, ip_repr: &IpRepr, repr: &TcpRepr) -> bool {
        if self.state == State::Closed {
            return false;
        }

        // If we're still listening for SYNs and the packet has an ACK, it cannot
        // be destined to this socket, but another one may well listen on the same
        // local endpoint.
        if self.state == State::Listen && repr.ack_number.is_some() {
            return false;
        }

        if let Some(tuple) = &self.tuple {
            // Reject packets not matching the 4-tuple
            ip_repr.dst_addr() == tuple.local.addr
                && repr.dst_port == tuple.local.port
                && ip_repr.src_addr() == tuple.remote.addr
                && repr.src_port == tuple.remote.port
        } else {
            // We're listening, reject packets not matching the listen endpoint.
            if self
                .listen_ip_version
                .is_some_and(|version| version != ip_repr.version())
            {
                return false;
            }
            let addr_ok = match self.listen_endpoint.addr {
                Some(addr) => ip_repr.dst_addr() == addr,
                None => true,
            };
            addr_ok && repr.dst_port != 0 && repr.dst_port == self.listen_endpoint.port
        }
    }

    pub fn process(
        &mut self,
        cx: &mut Context,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        self.process_with_meta(cx, PacketMeta::default(), ip_repr, repr)
    }

    pub(crate) fn process_with_meta(
        &mut self,
        cx: &mut Context,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        self.process_with_reuse(cx, meta, ip_repr, repr, None)
    }

    fn passive_open_device(&self, _ip: &IpRepr, _meta: PacketMeta) -> u32 {
        #[cfg(feature = "packetmeta-id")]
        {
            let device = self.listen_bound_device;
            #[cfg(feature = "proto-ipv6")]
            let device = device.or_else(|| match _ip.src_addr() {
                IpAddress::Ipv6(address)
                    if address.octets()[0] == 0xfe && address.octets()[1] & 0xc0 == 0x80 =>
                {
                    NonZeroU32::new(_meta.id)
                }
                _ => None,
            });
            device.map_or(0, NonZeroU32::get)
        }
        #[cfg(not(feature = "packetmeta-id"))]
        {
            0
        }
    }

    pub(crate) fn process_with_reuse(
        &mut self,
        cx: &mut Context,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
        reuse: Option<(&TimeWaitState, Option<u64>)>,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        if self.state == State::TimeWait {
            self.ensure_time_wait(cx.now());
            let tw = self.time_wait.as_mut().unwrap();
            let result = match tw.process(cx.now(), repr) {
                TimeWaitAction::Remove => {
                    self.retire_time_wait();
                    None
                }
                TimeWaitAction::Ack | TimeWaitAction::Reopen => tw.reply(cx.now(), repr),
                TimeWaitAction::Ignore => None,
            };
            if let Some(tw) = &self.time_wait {
                self.timer = Timer::Close {
                    expires_at: tw.expires,
                };
            }
            return result;
        }
        // Complete all rejecting checks before claiming an integration identity.
        if self.state != State::Listen && time_wait::paws_reject(self.ts_recent, repr, cx.now()) {
            return self.challenge_ack_reply(cx, ip_repr, repr);
        }
        let passive_device = self.passive_open_device(ip_repr, meta);
        if self.state == State::Listen
            && repr.control == TcpControl::Syn
            && repr.ack_number.is_none()
            && (repr.max_seg_size == Some(0)
                || !self.prepare_open_on_device(
                    IpEndpoint::new(ip_repr.dst_addr(), repr.dst_port),
                    IpEndpoint::new(ip_repr.src_addr(), repr.src_port),
                    passive_device,
                    reuse.and_then(|(_, id)| id),
                ))
        {
            return None;
        }
        let was_listen = self.state == State::Listen;
        let result = self.process_inner(cx, ip_repr, repr, passive_device);
        if was_listen && self.state == State::SynReceived {
            if let Some((old, _)) = reuse {
                self.local_seq_no = old.new_isn();
                self.remote_last_seq = self.local_seq_no;
                self.admitted_seq_end = None;
            }
        }
        if self.state == State::TimeWait {
            self.ensure_time_wait(cx.now());
        }
        self.notify_lifecycle();
        result
    }

    pub(crate) fn ensure_time_wait(&mut self, now: Instant) {
        if self.state == State::TimeWait && self.time_wait.is_none() {
            let mut state = TimeWaitState::from_socket(self, now);
            // Entering the state establishes the deadline once; subsequent
            // refreshes belong to TimeWaitState::process.
            state.expires = now + self.time_wait_duration;
            self.time_wait = Some(state);
        }
    }

    fn process_inner(
        &mut self,
        cx: &mut Context,
        ip_repr: &IpRepr,
        repr: &TcpRepr,
        _passive_device: u32,
    ) -> Option<(IpRepr, TcpRepr<'static>)> {
        debug_assert!(self.accepts(cx, ip_repr, repr));

        // Consider how much the sequence number space differs from the transmit buffer space.
        let (sent_syn, sent_fin) = match self.state {
            // In SYN-SENT or SYN-RECEIVED, we've just sent a SYN.
            State::SynSent | State::SynReceived => (true, false),
            // In FIN-WAIT-1, LAST-ACK, or CLOSING, we've just sent a FIN.
            State::FinWait1 | State::LastAck | State::Closing => (false, true),
            // In all other states we've already got acknowledgements for
            // all of the control flags we sent.
            _ => (false, false),
        };
        let control_len = (sent_syn as usize) + (sent_fin as usize);

        // Reject unacceptable acknowledgements.
        match (self.state, repr.control, repr.ack_number) {
            // An RST received in response to initial SYN is acceptable if it acknowledges
            // the initial SYN.
            (State::SynSent, TcpControl::Rst, None) => {
                net_debug!("unacceptable RST (expecting RST|ACK) in response to initial SYN");
                return None;
            }
            (State::SynSent, TcpControl::Rst, Some(ack_number)) => {
                if ack_number != self.local_seq_no + 1 {
                    net_debug!("unacceptable RST|ACK in response to initial SYN");
                    return None;
                }
            }
            // Any other RST need only have a valid sequence number.
            (_, TcpControl::Rst, _) => (),
            // The initial SYN cannot contain an acknowledgement.
            (State::Listen, _, None) => (),
            // This case is handled in `accepts()`.
            (State::Listen, _, Some(_)) => unreachable!(),
            // SYN|ACK in the SYN-SENT state must have the exact ACK number.
            (State::SynSent, TcpControl::Syn, Some(ack_number)) => {
                if ack_number != self.local_seq_no + 1 {
                    net_debug!("unacceptable SYN|ACK in response to initial SYN");
                    return Some(Self::rst_reply(ip_repr, repr));
                }
            }
            // TCP simultaneous open.
            // This is required by RFC 9293, which states "A TCP implementation MUST support
            // simultaneous open attempts (MUST-10)."
            (State::SynSent, TcpControl::Syn, None) => (),
            // ACKs in the SYN-SENT state are invalid.
            (State::SynSent, TcpControl::None, Some(ack_number)) => {
                // If the sequence number matches, ignore it instead of RSTing.
                // I'm not sure why, I think it may be a workaround for broken TCP
                // servers, or a defense against reordering. Either way, if Linux
                // does it, we do too.
                if ack_number == self.local_seq_no + 1 {
                    net_debug!(
                        "expecting a SYN|ACK, received an ACK with the right ack_number, ignoring."
                    );
                    return None;
                }

                net_debug!(
                    "expecting a SYN|ACK, received an ACK with the wrong ack_number, sending RST."
                );
                return Some(Self::rst_reply(ip_repr, repr));
            }
            // Anything else in the SYN-SENT state is invalid.
            (State::SynSent, _, _) => {
                net_debug!("expecting a SYN|ACK");
                return None;
            }
            // Every packet after the initial SYN must be an acknowledgement.
            (_, _, None) => {
                net_debug!("expecting an ACK");
                return None;
            }
            // ACK in the SYN-RECEIVED state must have the exact ACK number, or we RST it.
            (State::SynReceived, _, Some(ack_number)) => {
                if ack_number != self.local_seq_no + 1 {
                    net_debug!("unacceptable ACK in response to SYN|ACK");
                    return Some(Self::rst_reply(ip_repr, repr));
                }
            }
            // Every acknowledgement must be for transmitted but unacknowledged data.
            (_, _, Some(ack_number)) => {
                let unacknowledged = self.tx_buffer.len() + control_len;

                // Acceptable ACK range (both inclusive)
                let mut ack_min = self.local_seq_no;
                let ack_max = self.local_seq_no + unacknowledged;

                // If we have sent a SYN, it MUST be acknowledged.
                if sent_syn {
                    ack_min += 1;
                }

                if ack_number < ack_min {
                    net_debug!(
                        "duplicate ACK ({} not in {}...{})",
                        ack_number,
                        ack_min,
                        ack_max
                    );
                    return None;
                }

                if ack_number > ack_max {
                    net_debug!(
                        "unacceptable ACK ({} not in {}...{})",
                        ack_number,
                        ack_min,
                        ack_max
                    );
                    return self.challenge_ack_reply(cx, ip_repr, repr);
                }
            }
        }

        let window_start = self.remote_seq_no + self.rx_buffer.len();
        let window_end = self.remote_seq_no + self.rx_buffer.capacity();
        let segment_start = repr.seq_number;
        let segment_end = repr.seq_number + repr.payload.len();

        if self.should_reset_receive_shutdown_data(segment_start, segment_end, window_start) {
            net_debug!(
                "received data after receive shutdown in {}, sending RST",
                self.state
            );
            self.set_state(State::Closed);
            return Some(Self::rst_reply(ip_repr, repr));
        }

        let (payload, payload_offset) = match self.state {
            // In LISTEN and SYN-SENT states, we have not yet synchronized with the remote end.
            State::Listen | State::SynSent => (&[][..], 0),
            _ => {
                // https://www.rfc-editor.org/rfc/rfc9293.html#name-segment-acceptability-tests
                let segment_in_window = match (
                    segment_start == segment_end,
                    window_start == window_end,
                ) {
                    (true, _) if segment_end == window_start - 1 => {
                        net_debug!(
                            "received a keep-alive or window probe packet, will send an ACK"
                        );
                        false
                    }
                    (true, true) => {
                        if window_start == segment_start {
                            true
                        } else {
                            net_debug!(
                                "zero-length segment not inside zero-length window, will send an ACK."
                            );
                            false
                        }
                    }
                    (true, false) => {
                        if window_start <= segment_start && segment_start < window_end {
                            true
                        } else {
                            net_debug!("zero-length segment not inside window, will send an ACK.");
                            false
                        }
                    }
                    (false, true) => {
                        net_debug!(
                            "non-zero-length segment with zero receive window, will only send an ACK"
                        );
                        false
                    }
                    (false, false) => {
                        if (window_start <= segment_start && segment_start < window_end)
                            || (window_start < segment_end && segment_end <= window_end)
                        {
                            true
                        } else {
                            net_debug!(
                                "segment not in receive window ({}..{} not intersecting {}..{}), will send challenge ACK",
                                segment_start,
                                segment_end,
                                window_start,
                                window_end
                            );
                            false
                        }
                    }
                };

                if segment_in_window {
                    let overlap_start = window_start.max(segment_start);
                    let overlap_end = window_end.min(segment_end);

                    // the checks done above imply this.
                    debug_assert!(overlap_start <= overlap_end);

                    self.local_rx_last_seq = Some(repr.seq_number);

                    (
                        &repr.payload[overlap_start - segment_start..overlap_end - segment_start],
                        overlap_start - window_start,
                    )
                } else {
                    // If we're in the TIME-WAIT state, restart the TIME-WAIT timeout, since
                    // the remote end may not have realized we've closed the connection.
                    if self.state == State::TimeWait {
                        self.timer.set_for_close(cx.now());
                    }

                    return self.challenge_ack_reply(cx, ip_repr, repr);
                }
            }
        };

        // Compute the amount of acknowledged octets, removing the SYN and FIN bits
        // from the sequence space.
        let mut ack_len = 0;
        let mut ack_of_fin = false;
        let mut ack_all = false;
        if repr.control != TcpControl::Rst {
            if let Some(ack_number) = repr.ack_number {
                // A valid current ACK demonstrates a working path. Linux
                // tcp_ack clears sk_err_soft here, not in user I/O itself.
                if ack_number >= self.local_seq_no {
                    self.soft_error = None;
                }
                // Sequence number corresponding to the first byte in `tx_buffer`.
                // This normally equals `local_seq_no`, but is 1 higher if we have sent a SYN,
                // as the SYN occupies 1 sequence number "before" the data.
                let tx_buffer_start_seq = self.local_seq_no + (sent_syn as usize);

                if ack_number >= tx_buffer_start_seq {
                    ack_len = ack_number - tx_buffer_start_seq;

                    // We could've sent data before the FIN, so only remove FIN from the sequence
                    // space if all of that data is acknowledged.
                    if sent_fin && self.tx_buffer.len() + 1 == ack_len {
                        ack_len -= 1;
                        tcp_trace!("received ACK of FIN");
                        ack_of_fin = true;
                    }

                    ack_all = self.send_sequence_end() == ack_number
                }

                self.rtte.on_ack(cx.now(), ack_number);
                self.congestion_controller
                    .inner_mut()
                    .on_ack(cx.now(), ack_len, &self.rtte);
            }
        }

        // Disregard control flags we don't care about or shouldn't act on yet.
        let mut control = repr.control;
        control = control.quash_psh();

        // If a FIN is received at the end of the current segment, but
        // we have a hole in the assembler before the current segment, disregard this FIN.
        if control == TcpControl::Fin && window_start < segment_start {
            tcp_trace!("ignoring FIN because we don't have full data yet. window_start={} segment_start={}", window_start, segment_start);
            control = TcpControl::None;
        }

        // Validate and update the state.
        match (self.state, control) {
            // RSTs are not accepted in the LISTEN state.
            (State::Listen, TcpControl::Rst) => return None,

            // RSTs in SYN-RECEIVED flip the socket back to the LISTEN state.
            // Here we need to additionally check `listen_endpoint`, because we want to make sure
            // that SYN-RECEIVED was actually converted from the LISTEN state (another possible
            // reason is TCP simultaneous open).
            (State::SynReceived, TcpControl::Rst) if self.listen_endpoint.port != 0 => {
                tcp_trace!("received RST");
                self.tuple = None;
                // A recycled listen slot has no peer clock or ACK sequence.
                // These belong to the rejected connection, not the listener.
                self.ts_recent = None;
                self.last_remote_tsval = 0;
                self.tsval_generator = self.configured_tsval_generator;
                self.remote_last_ack = None;
                self.set_state(State::Listen);
                return None;
            }

            // RSTs in any other state close the socket.
            (_, TcpControl::Rst) => {
                tcp_trace!("received RST");
                self.pending_error = Some(PendingError {
                    reason: ErrorReason::Reset,
                    state: self.state,
                });
                self.set_state(State::Closed);
                self.tuple = None;
                return None;
            }

            // SYN packets in the LISTEN state change it to SYN-RECEIVED.
            (State::Listen, TcpControl::Syn) => {
                tcp_trace!("received SYN");
                if let Some(max_seg_size) = repr.max_seg_size {
                    if max_seg_size == 0 {
                        tcp_trace!("received SYNACK with zero MSS, ignoring");
                        return None;
                    }
                    self.congestion_controller
                        .inner_mut()
                        .set_mss(max_seg_size as usize);
                    self.remote_mss = max_seg_size as usize
                }

                self.recv_error_policies = self.listen_recv_error_policies;
                if let Some(policies) = self.listen_pmtu_policies {
                    self.pmtu_discover = policies[match ip_repr {
                        #[cfg(feature = "proto-ipv4")]
                        IpRepr::Ipv4(_) => 0,
                        #[cfg(feature = "proto-ipv6")]
                        IpRepr::Ipv6(_) => 1,
                    }];
                }

                #[cfg(feature = "packetmeta-id")]
                {
                    self.connection_bound_device = NonZeroU32::new(_passive_device);
                }
                self.tuple = Some(Tuple {
                    local: IpEndpoint::new(ip_repr.dst_addr(), repr.dst_port),
                    remote: IpEndpoint::new(ip_repr.src_addr(), repr.src_port),
                });
                self.local_seq_no = Self::random_seq_no(cx);
                self.admitted_seq_end = None;
                self.remote_seq_no = repr.seq_number + 1;
                self.remote_last_seq = self.local_seq_no;
                self.remote_has_sack = repr.sack_permitted;
                self.remote_win_scale = repr.window_scale;
                // Remote doesn't support window scaling, don't do it.
                if self.remote_win_scale.is_none() {
                    self.remote_win_shift = 0;
                }
                // Remote doesn't support timestamping, don't do it.
                if repr.timestamp.is_none() {
                    self.tsval_generator = None;
                }
                self.set_state(State::SynReceived);
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // ACK packets in the SYN-RECEIVED state change it to ESTABLISHED.
            (State::SynReceived, TcpControl::None) => {
                self.set_state(State::Established);
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // FIN packets in the SYN-RECEIVED state change it to CLOSE-WAIT.
            // It's not obvious from RFC 793 that this is permitted, but
            // 7th and 8th steps in the "SEGMENT ARRIVES" event describe this behavior.
            (State::SynReceived, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                self.set_state(State::CloseWait);
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // SYN|ACK packets in the SYN-SENT state change it to ESTABLISHED.
            // SYN packets in the SYN-SENT state change it to SYN-RECEIVED.
            (State::SynSent, TcpControl::Syn) => {
                if repr.ack_number.is_some() {
                    tcp_trace!("received SYN|ACK");
                } else {
                    tcp_trace!("received SYN");
                }
                if let Some(max_seg_size) = repr.max_seg_size {
                    if max_seg_size == 0 {
                        tcp_trace!("received SYNACK with zero MSS, ignoring");
                        return None;
                    }
                    self.remote_mss = max_seg_size as usize;
                    self.congestion_controller
                        .inner_mut()
                        .set_mss(self.remote_mss);
                }

                self.remote_seq_no = repr.seq_number + 1;
                self.remote_last_seq = self.local_seq_no + 1;
                self.remote_last_ack = Some(repr.seq_number);
                self.remote_has_sack = repr.sack_permitted;
                self.remote_win_scale = repr.window_scale;
                // Remote doesn't support window scaling, don't do it.
                if self.remote_win_scale.is_none() {
                    self.remote_win_shift = 0;
                }
                // Remote doesn't support timestamping, don't do it.
                if repr.timestamp.is_none() {
                    self.tsval_generator = None;
                }

                if repr.ack_number.is_some() {
                    self.set_state(State::Established);
                } else {
                    self.set_state(State::SynReceived);
                }
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // ACK packets in ESTABLISHED state reset the retransmit timer,
            // except for duplicate ACK packets which preserve it.
            (State::Established, TcpControl::None) => {
                if !self.timer.is_retransmit() || ack_all {
                    self.timer.set_for_idle(cx.now(), self.keep_alive);
                }
            }

            // FIN packets in ESTABLISHED state indicate the remote side has closed.
            (State::Established, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                self.set_state(State::CloseWait);
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // ACK packets in FIN-WAIT-1 state change it to FIN-WAIT-2, if we've already
            // sent everything in the transmit buffer. If not, they reset the retransmit timer.
            (State::FinWait1, TcpControl::None) => {
                if ack_of_fin {
                    self.set_state(State::FinWait2);
                }
                if ack_all {
                    self.timer.set_for_idle(cx.now(), self.keep_alive);
                }
            }

            // FIN packets in FIN-WAIT-1 state change it to CLOSING, or to TIME-WAIT
            // if they also acknowledge our FIN.
            (State::FinWait1, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                if ack_of_fin {
                    self.set_state(State::TimeWait);
                    self.timer.set_for_close(cx.now());
                } else {
                    self.set_state(State::Closing);
                    self.timer.set_for_idle(cx.now(), self.keep_alive);
                }
            }

            // Data packets in FIN-WAIT-2 reset the idle timer.
            (State::FinWait2, TcpControl::None) => {
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // FIN packets in FIN-WAIT-2 state change it to TIME-WAIT.
            (State::FinWait2, TcpControl::Fin) => {
                self.remote_seq_no += 1;
                self.rx_fin_received = true;
                self.set_state(State::TimeWait);
                self.timer.set_for_close(cx.now());
            }

            // ACK packets in CLOSING state change it to TIME-WAIT.
            (State::Closing, TcpControl::None) => {
                if ack_of_fin {
                    self.set_state(State::TimeWait);
                    self.timer.set_for_close(cx.now());
                } else {
                    self.timer.set_for_idle(cx.now(), self.keep_alive);
                }
            }

            // ACK packets in CLOSE-WAIT state reset the retransmit timer.
            (State::CloseWait, TcpControl::None) => {
                self.timer.set_for_idle(cx.now(), self.keep_alive);
            }

            // ACK packets in LAST-ACK state change it to CLOSED.
            (State::LastAck, TcpControl::None) => {
                if ack_of_fin {
                    // Clear the remote endpoint, or we'll send an RST there.
                    self.set_state(State::Closed);
                    self.tuple = None;
                } else {
                    self.timer.set_for_idle(cx.now(), self.keep_alive);
                }
            }

            _ => {
                net_debug!("unexpected packet {}", repr);
                return None;
            }
        }

        // Update remote state.
        self.remote_last_ts = Some(cx.now());

        // RFC 1323: The window field (SEG.WND) in the header of every incoming segment, with the
        // exception of SYN segments, is left-shifted by Snd.Wind.Scale bits before updating SND.WND.
        let scale = match repr.control {
            TcpControl::Syn => 0,
            _ => self.remote_win_scale.unwrap_or(0),
        };
        let new_remote_win_len = (repr.window_len as usize) << (scale as usize);
        let is_window_update = new_remote_win_len != self.remote_win_len;
        self.remote_win_len = new_remote_win_len;

        self.congestion_controller
            .inner_mut()
            .set_remote_window(new_remote_win_len);

        if ack_len > 0 {
            // Dequeue acknowledged octets.
            debug_assert!(self.tx_buffer.len() >= ack_len);
            tcp_trace!(
                "tx buffer: dequeueing {} octets (now {})",
                ack_len,
                self.tx_buffer.len() - ack_len
            );
            self.tx_buffer.dequeue_allocated(ack_len);

            // There's new room available in tx_buffer, wake the waiting task if any.
            #[cfg(feature = "async")]
            self.tx_waker.wake();
        }

        if let Some(ack_number) = repr.ack_number {
            // TODO: When flow control is implemented,
            // refractor the following block within that implementation

            // Detect and react to duplicate ACKs by:
            // 1. Check if duplicate ACK and change self.local_rx_dup_acks accordingly
            // 2. If exactly 3 duplicate ACKs received, set for fast retransmit
            // 3. Update the last received ACK (self.local_rx_last_ack)
            match self.local_rx_last_ack {
                // Duplicate ACK if payload empty and ACK doesn't move send window ->
                // Increment duplicate ACK count and set for retransmit if we just received
                // the third duplicate ACK
                Some(last_rx_ack)
                    if repr.payload.is_empty()
                        && last_rx_ack == ack_number
                        && ack_number < self.send_sequence_end()
                        && !is_window_update =>
                {
                    // Increment duplicate ACK count
                    self.local_rx_dup_acks = self.local_rx_dup_acks.saturating_add(1);

                    // Inform congestion controller of duplicate ACK
                    self.congestion_controller
                        .inner_mut()
                        .on_duplicate_ack(cx.now());

                    net_debug!(
                        "received duplicate ACK for seq {} (duplicate nr {}{})",
                        ack_number,
                        self.local_rx_dup_acks,
                        if self.local_rx_dup_acks == u8::MAX {
                            "+"
                        } else {
                            ""
                        }
                    );

                    if self.local_rx_dup_acks == 3 {
                        self.timer.set_for_fast_retransmit();
                        net_debug!("started fast retransmit");
                    }
                }
                // No duplicate ACK -> Reset state and update last received ACK
                _ => {
                    if self.local_rx_dup_acks > 0 {
                        self.local_rx_dup_acks = 0;
                        net_debug!("reset duplicate ACK count");
                    }
                    self.local_rx_last_ack = Some(ack_number);
                }
            };
            // We've processed everything in the incoming segment, so advance the local
            // sequence number past it.
            self.local_seq_no = ack_number;
            if self.tx_urgent_end.is_some_and(|end| ack_number >= end) {
                self.tx_urgent_end = None;
            }
            // During retransmission, if an earlier segment got lost but later was
            // successfully received, self.local_seq_no can move past self.remote_last_seq.
            // Do not attempt to retransmit the latter segments; not only this is pointless
            // in theory but also impossible in practice, since they have been already
            // deallocated from the buffer.
            if self.remote_last_seq < self.local_seq_no {
                self.remote_last_seq = self.local_seq_no
            }
        }

        // update last remote tsval
        if let Some(timestamp) = repr.timestamp {
            self.last_remote_tsval = timestamp.tsval;
            if self.tsval_generator.is_some()
                && self
                    .remote_last_ack
                    .map_or(true, |ack| repr.seq_number <= ack)
            {
                self.ts_recent = Some((timestamp.tsval, cx.now()));
            }
        }

        if !matches!(
            self.state,
            State::Listen | State::SynSent | State::SynReceived | State::Closed | State::TimeWait
        ) {
            self.process_urgent(repr);
        }

        let payload_len = payload.len();
        if payload_len == 0 {
            return None;
        }

        let assembler_was_empty = self.assembler.is_empty();

        // Try adding payload octets to the assembler.
        let Ok(contig_len) = self
            .assembler
            .add_then_remove_front(payload_offset, payload_len)
        else {
            net_debug!(
                "assembler: too many holes to add {} octets at offset {}",
                payload_len,
                payload_offset
            );
            return None;
        };

        // Place payload octets into the buffer.
        tcp_trace!(
            "rx buffer: receiving {} octets at offset {}",
            payload_len,
            payload_offset
        );
        let len_written = self.rx_buffer.write_unallocated(payload_offset, payload);
        debug_assert!(len_written == payload_len);

        if contig_len != 0 {
            // Enqueue the contiguous data octets in front of the buffer.
            tcp_trace!(
                "rx buffer: enqueueing {} octets (now {})",
                contig_len,
                self.rx_buffer.len() + contig_len
            );
            self.rx_buffer.enqueue_unallocated(contig_len);

            // There's new data in rx_buffer, notify waiting task if any.
            #[cfg(feature = "async")]
            self.rx_waker.wake();
        }

        if !self.assembler.is_empty() {
            // Print the ranges recorded in the assembler.
            tcp_trace!("assembler: {}", self.assembler);
        }

        // Handle delayed acks
        if let Some(ack_delay) = self.ack_delay {
            if self.ack_to_transmit() {
                self.ack_delay_timer = match self.ack_delay_timer {
                    AckDelayTimer::Idle => {
                        tcp_trace!("starting delayed ack timer");
                        AckDelayTimer::Waiting(cx.now() + ack_delay)
                    }
                    AckDelayTimer::Waiting(_) if self.immediate_ack_to_transmit() => {
                        tcp_trace!("delayed ack timer already started, forcing expiry");
                        AckDelayTimer::Immediate
                    }
                    timer @ AckDelayTimer::Waiting(_) => {
                        tcp_trace!("waiting until delayed ack timer expires");
                        timer
                    }
                    AckDelayTimer::Immediate => {
                        tcp_trace!("delayed ack timer already force-expired");
                        AckDelayTimer::Immediate
                    }
                };
            }
        }

        // Per RFC 5681, we should send an immediate ACK when either:
        //  1) an out-of-order segment is received, or
        //  2) a segment arrives that fills in all or part of a gap in sequence space.
        if !self.assembler.is_empty() || !assembler_was_empty {
            // Note that we change the transmitter state here.
            // This is fine because smoltcp assumes that it can always transmit zero or one
            // packets for every packet it receives.
            tcp_trace!("ACKing incoming segment");
            Some(self.ack_reply(ip_repr, repr))
        } else {
            None
        }
    }

    fn timed_out(&self, timestamp: Instant) -> bool {
        match (self.remote_last_ts, self.timeout) {
            (Some(remote_last_ts), Some(timeout)) => timestamp >= remote_last_ts + timeout,
            (_, _) => false,
        }
    }

    fn seq_to_transmit(&self, cx: &mut Context) -> bool {
        let ip_header_len = match self.tuple.unwrap().local.addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(_) => crate::wire::IPV4_HEADER_LEN,
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(_) => crate::wire::IPV6_HEADER_LEN,
        };

        // Max segment size we're able to send due to MTU limitations.
        let tcp_header_len = TCP_HEADER_LEN
            + if self.tsval_generator.is_some() {
                12
            } else {
                0
            };
        let local_mss = self
            .output_ip_mtu
            .unwrap_or_else(|| cx.ip_mtu())
            .saturating_sub(ip_header_len + tcp_header_len);

        // The effective max segment size, taking into account our and remote's limits.
        let effective_mss = local_mss.min(self.remote_mss);

        // Have we sent data that hasn't been ACKed yet?
        let data_in_flight = self.remote_last_seq != self.local_seq_no;

        // If we want to send a SYN and we haven't done so, do it!
        if matches!(self.state, State::SynSent | State::SynReceived) && !data_in_flight {
            return true;
        }

        // max sequence number we can send.
        let max_send_seq =
            self.local_seq_no + core::cmp::min(self.remote_win_len, self.tx_buffer.len());

        // Max amount of octets we can send.
        let max_send = if max_send_seq >= self.remote_last_seq {
            max_send_seq - self.remote_last_seq
        } else {
            0
        };

        // Compare max_send with the congestion window.
        let max_send = max_send.min(self.congestion_controller.inner().window());

        // Can we send at least 1 octet?
        let mut can_send = max_send != 0;
        // Can we send at least 1 full segment?
        let can_send_full = max_send >= effective_mss;

        // Do we have to send a FIN?
        let want_fin = match self.state {
            State::FinWait1 => true,
            State::Closing => true,
            State::LastAck => true,
            _ => false,
        };

        // If we're applying the Nagle algorithm we don't want to send more
        // until one of:
        // * There's no data in flight
        // * We can send a full packet
        // * We have all the data we'll ever send (we're closing send)
        if self.nagle
            && self.tx_urgent_end.is_none()
            && data_in_flight
            && !can_send_full
            && !want_fin
        {
            can_send = false;
        }

        // Can we actually send the FIN? We can send it if:
        // 1. We have unsent data that fits in the remote window.
        // 2. We have no unsent data.
        // This condition matches only if #2, because #1 is already covered by can_data and we're ORing them.
        let can_fin = want_fin && self.remote_last_seq == self.local_seq_no + self.tx_buffer.len();

        can_send || can_fin
    }

    fn delayed_ack_expired(&self, timestamp: Instant) -> bool {
        match self.ack_delay_timer {
            AckDelayTimer::Idle => true,
            AckDelayTimer::Waiting(t) => t <= timestamp,
            AckDelayTimer::Immediate => true,
        }
    }

    pub(crate) fn ack_to_transmit(&self) -> bool {
        if let Some(remote_last_ack) = self.remote_last_ack {
            remote_last_ack < self.remote_seq_no + self.rx_buffer.len()
        } else {
            false
        }
    }

    /// Return whether to send ACK immediately due to the amount of unacknowledged data.
    ///
    /// RFC 9293 states "An ACK SHOULD be generated for at least every second full-sized segment or
    /// 2*RMSS bytes of new data (where RMSS is the MSS specified by the TCP endpoint receiving the
    /// segments to be acknowledged, or the default value if not specified) (SHLD-19)."
    ///
    /// Note that the RFC above only says "at least 2*RMSS bytes", which is not a hard requirement.
    /// In practice, we follow the Linux kernel's empirical value of sending an ACK for every RMSS
    /// byte of new data. For details, see
    /// <https://elixir.bootlin.com/linux/v6.11.4/source/net/ipv4/tcp_input.c#L5747>.
    fn immediate_ack_to_transmit(&self) -> bool {
        if let Some(remote_last_ack) = self.remote_last_ack {
            remote_last_ack + self.remote_mss < self.remote_seq_no + self.rx_buffer.len()
        } else {
            false
        }
    }

    /// Return whether we should send ACK immediately due to significant window updates.
    ///
    /// ACKs with significant window updates should be sent immediately to let the sender know that
    /// more data can be sent. According to the Linux kernel implementation, "significant" means
    /// doubling the receive window. The Linux kernel implementation can be found at
    /// <https://elixir.bootlin.com/linux/v6.9.9/source/net/ipv4/tcp.c#L1472>.
    fn window_to_update(&self) -> bool {
        match self.state {
            // SYN windows are unscaled and the handshake is driven by SYN
            // sequence/timer state, not ordinary scaled ACK window updates.
            // Comparing those two wire formats could spuriously resend SYNACK.
            State::Established | State::FinWait1 | State::FinWait2 => {
                let new_win = self.scaled_window();
                if let Some(last_win) = self.last_scaled_window() {
                    new_win > 0 && new_win / 2 >= last_win
                } else {
                    false
                }
            }
            _ => false,
        }
    }

    pub fn dispatch<F, E>(&mut self, cx: &mut Context, emit: F) -> Result<(), E>
    where
        F: FnOnce(&mut Context, (IpRepr, TcpRepr)) -> Result<(), E>,
    {
        self.dispatch_with_mtu(cx, cx.ip_mtu(), emit)
    }

    /// Dispatch using the current output route's IP MTU. This does not reserve
    /// a device token, so timer-only state transitions remain independent of TX.
    pub fn dispatch_with_mtu<F, E>(
        &mut self,
        cx: &mut Context,
        ip_mtu: usize,
        emit: F,
    ) -> Result<(), E>
    where
        F: FnOnce(&mut Context, (IpRepr, TcpRepr)) -> Result<(), E>,
    {
        self.policy_retry_at = None;
        self.policy_retry_failures = 0;
        let result = self.dispatch_inner(cx, ip_mtu, 0, |cx, packet| {
            emit(cx, packet).map(|_| TcpPacketAdmission::Admitted)
        });
        self.notify_lifecycle();
        result.map(|_| ())
    }

    /// Like `dispatch_with_mtu`, but report policy denial before treating a
    /// TCP packet as sent. Only sequence-bearing data/FIN is deferred; SYN and
    /// one-shot control packets retain their normal timer/state transitions.
    pub fn dispatch_with_mtu_policy<F, E>(
        &mut self,
        cx: &mut Context,
        ip_mtu: usize,
        emit: F,
    ) -> Result<TcpDispatchOutcome, E>
    where
        F: FnOnce(&mut Context, (IpRepr, TcpRepr)) -> Result<TcpPacketAdmission, E>,
    {
        self.dispatch_with_mtu_policy_generation(cx, ip_mtu, 0, emit)
    }

    /// A generation ties final-route MTU retry hints to the integration's
    /// routing and output-policy view. It must change when that view changes.
    pub fn dispatch_with_mtu_policy_generation<F, E>(
        &mut self,
        cx: &mut Context,
        ip_mtu: usize,
        generation: u64,
        emit: F,
    ) -> Result<TcpDispatchOutcome, E>
    where
        F: FnOnce(&mut Context, (IpRepr, TcpRepr)) -> Result<TcpPacketAdmission, E>,
    {
        if self.policy_retry_at.is_some_and(|deadline| {
            cx.now() >= deadline || self.timer.should_retransmit(cx.now()).is_some()
        }) {
            self.policy_retry_at = None;
        }
        let mut admitted_sequence = false;
        let result = self.dispatch_inner(cx, ip_mtu, generation, |cx, packet| {
            let occupies_sequence = packet.1.segment_len() > 0;
            let admission = emit(cx, packet)?;
            admitted_sequence = occupies_sequence && admission == TcpPacketAdmission::Admitted;
            Ok(admission)
        });
        if matches!(result.as_ref(), Ok(TcpDispatchOutcome::PolicyDeferred)) {
            let shift = self.policy_retry_failures.min(10);
            let delay_ms = self
                .rto()
                .total_millis()
                .max(POLICY_PROBE_MIN_MS)
                .saturating_mul(1u64 << shift)
                .min(POLICY_PROBE_MAX_MS);
            self.policy_retry_at = Some(cx.now() + Duration::from_millis(delay_ms));
            self.policy_retry_failures = self.policy_retry_failures.saturating_add(1);
        } else if admitted_sequence {
            self.policy_retry_at = None;
            self.policy_retry_failures = 0;
        }
        self.notify_lifecycle();
        result
    }

    fn dispatch_inner<F, E>(
        &mut self,
        cx: &mut Context,
        ip_mtu: usize,
        generation: u64,
        emit: F,
    ) -> Result<TcpDispatchOutcome, E>
    where
        F: FnOnce(&mut Context, (IpRepr, TcpRepr)) -> Result<TcpPacketAdmission, E>,
    {
        if self.state == State::TimeWait {
            self.ensure_time_wait(cx.now());
            if self.time_wait.as_ref().unwrap().expires <= cx.now() {
                self.retire_time_wait();
                return Ok(TcpDispatchOutcome::Done);
            }
            self.timer = Timer::Close {
                expires_at: self.time_wait.as_ref().unwrap().expires,
            };
        }
        if self.tuple.is_none() {
            self.output_ip_mtu = None;
            return Ok(TcpDispatchOutcome::Done);
        }
        if self
            .mtu_retry_hint
            .is_some_and(|(old_generation, _)| old_generation != generation)
        {
            self.mtu_retry_hint = None;
            self.mtu_retry_at = None;
            self.mtu_retry_failures = 0;
        }
        let ip_mtu = self
            .mtu_retry_hint
            .map_or(ip_mtu, |(_, hint)| hint.min(ip_mtu));
        self.output_ip_mtu = Some(ip_mtu);
        if self.mtu_retry_at.is_some_and(|at| cx.now() >= at) {
            self.mtu_retry_at = None;
        }

        if self.remote_last_ts.is_none() {
            // We get here in exactly two cases:
            //  1) This socket just transitioned into SYN-SENT.
            //  2) This socket had an empty transmit buffer and some data was added there.
            // Both are similar in that the socket has been quiet for an indefinite
            // period of time, it isn't anymore, and the local endpoint is talking.
            // So, we start counting the timeout not from the last received packet
            // but from the first transmitted one.
            self.remote_last_ts = Some(cx.now());
        }

        self.congestion_controller
            .inner_mut()
            .pre_transmit(cx.now());

        // Check if any state needs to be changed because of a timer.
        if self.timed_out(cx.now()) {
            // If a timeout expires, we should abort the connection.
            net_debug!("timeout exceeded");
            self.pending_error = Some(PendingError {
                reason: self
                    .soft_error
                    .take()
                    .map_or(ErrorReason::TimedOut, ErrorReason::Network),
                state: self.state,
            });
            self.set_state(State::Closed);
        } else if let Some(retransmit_delta) = self.timer.should_retransmit(cx.now()) {
            // If a retransmit timer expired, we should resend data starting at the last ACK.
            net_debug!("retransmitting at t+{}", retransmit_delta);

            // Rewind "last sequence number sent", as if we never
            // had sent them. This will cause all data in the queue
            // to be sent again.
            self.remote_last_seq = self.local_seq_no;

            // Clear the `should_retransmit` state. If we can't retransmit right
            // now for whatever reason (like zero window), this avoids an
            // infinite polling loop where `poll_at` returns `Now` but `dispatch`
            // can't actually do anything.
            self.timer.set_for_idle(cx.now(), self.keep_alive);

            // Inform RTTE, so that it can avoid bogus measurements.
            self.rtte.on_retransmit();

            // Inform the congestion controller that we're retransmitting.
            self.congestion_controller
                .inner_mut()
                .on_retransmit(cx.now());
        }

        let defer_unsent_data = self
            .policy_retry_at
            .is_some_and(|deadline| cx.now() < deadline)
            || self
                .mtu_retry_at
                .is_some_and(|deadline| cx.now() < deadline);

        if self.mtu_retry_at.is_some_and(|at| cx.now() < at) {
            return Ok(TcpDispatchOutcome::Done);
        }

        // Decide whether we're sending a packet.
        if self.seq_to_transmit(cx) && !defer_unsent_data {
            // If we have data to transmit and it fits into partner's window, do it.
            tcp_trace!("outgoing segment will send data or flags");
        } else if self.ack_to_transmit() && self.delayed_ack_expired(cx.now()) {
            // If we have data to acknowledge, do it.
            tcp_trace!("outgoing segment will acknowledge");
        } else if self.window_to_update() {
            // If we have window length increase to advertise, do it.
            tcp_trace!("outgoing segment will update window");
        } else if self.state == State::Closed {
            // If we need to abort the connection, do it.
            tcp_trace!("outgoing segment will abort connection");
        } else if self.timer.should_keep_alive(cx.now()) {
            // If we need to transmit a keep-alive packet, do it.
            tcp_trace!("keep-alive timer expired");
        } else if self.timer.should_close(cx.now()) {
            // If we have spent enough time in the TIME-WAIT state, close the socket.
            tcp_trace!("TIME-WAIT timer expired");
            self.reset();
            return Ok(TcpDispatchOutcome::Done);
        } else {
            return Ok(TcpDispatchOutcome::Done);
        }

        // NOTE(unwrap): we check tuple is not None the first thing in this function.
        let tuple = self.tuple.unwrap();

        // Construct the lowered IP representation.
        // We might need this to calculate the MSS, so do it early.
        let mut ip_repr = IpRepr::new(
            tuple.local.addr,
            tuple.remote.addr,
            IpProtocol::Tcp,
            0,
            self.hop_limit.unwrap_or(64),
        );

        // Construct the basic TCP representation, an empty ACK packet.
        // We'll adjust this to be more specific as needed.
        let mut repr = TcpRepr {
            src_port: tuple.local.port,
            dst_port: tuple.remote.port,
            control: TcpControl::None,
            seq_number: self.remote_last_seq,
            ack_number: Some(self.remote_seq_no + self.rx_buffer.len()),
            urgent_at: None,
            window_len: self.scaled_window(),
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: TcpTimestampRepr::generate_reply_with_tsval(
                self.tsval_generator,
                self.last_remote_tsval,
            ),
            payload: &[],
        };

        match self.state {
            // We transmit an RST in the CLOSED state. If we ended up in the CLOSED state
            // with a specified endpoint, it means that the socket was aborted.
            State::Closed => {
                repr.control = TcpControl::Rst;
            }

            // We never transmit anything in the LISTEN state.
            State::Listen => return Ok(TcpDispatchOutcome::Done),

            // We transmit a SYN in the SYN-SENT state.
            // We transmit a SYN|ACK in the SYN-RECEIVED state.
            State::SynSent | State::SynReceived => {
                repr.control = TcpControl::Syn;
                repr.seq_number = self.local_seq_no;
                // window len must NOT be scaled in SYNs.
                repr.window_len = u16::try_from(self.desired_receive_window()).unwrap_or(u16::MAX);
                if self.state == State::SynSent {
                    repr.ack_number = None;
                    repr.window_scale = Some(self.remote_win_shift);
                    repr.sack_permitted = true;
                } else {
                    repr.sack_permitted = self.remote_has_sack;
                    repr.window_scale = self.remote_win_scale.map(|_| self.remote_win_shift);
                }
            }

            // We transmit data in all states where we may have data in the buffer,
            // or the transmit half of the connection is still open.
            State::Established
            | State::FinWait1
            | State::Closing
            | State::CloseWait
            | State::LastAck => {
                // Ensure remote_last_seq is at least local_seq_no.
                // This can happen if we receive an ACK for data we haven't sent yet
                // (which is invalid but shouldn't crash us), or if the remote side
                // has acknowledged data that we were about to retransmit.
                if self.remote_last_seq < self.local_seq_no {
                    self.remote_last_seq = self.local_seq_no;
                    repr.seq_number = self.remote_last_seq;
                }

                // Extract as much data as the remote side can receive in this packet
                // from the transmit buffer.

                // Right edge of window, ie the max sequence number we're allowed to send.
                let win_right_edge = self.local_seq_no + self.remote_win_len;

                // Max amount of octets we're allowed to send according to the remote window.
                let win_limit = if win_right_edge >= self.remote_last_seq {
                    win_right_edge - self.remote_last_seq
                } else {
                    // This can happen if we've sent some data and later the remote side
                    // has shrunk its window so that data is no longer inside the window.
                    // This should be very rare and is strongly discouraged by the RFCs,
                    // but it does happen in practice.
                    // http://www.tcpipguide.com/free/t_TCPWindowManagementIssues.htm
                    0
                };

                // Maximum size we're allowed to send. This can be limited by 3 factors:
                // 1. remote window
                // 2. MSS the remote is willing to accept, probably determined by their MTU
                // 3. MSS we can send, determined by our MTU.
                let size = win_limit
                    .min(self.remote_mss)
                    .min(ip_mtu.saturating_sub(ip_repr.header_len() + repr.header_len()));

                let offset = self.remote_last_seq - self.local_seq_no;
                if !defer_unsent_data {
                    repr.payload = self.tx_buffer.get_allocated(offset, size);
                }

                // If we've sent everything we had in the buffer, follow it with the PSH or FIN
                // flags, depending on whether the transmit half of the connection is open.
                if !defer_unsent_data && offset + repr.payload.len() == self.tx_buffer.len() {
                    match self.state {
                        State::FinWait1 | State::LastAck | State::Closing => {
                            repr.control = TcpControl::Fin
                        }
                        State::Established | State::CloseWait if !repr.payload.is_empty() => {
                            repr.control = TcpControl::Psh
                        }
                        _ => (),
                    }
                }
            }

            // In FIN-WAIT-2 and TIME-WAIT states we may only transmit ACKs for incoming data or FIN
            State::FinWait2 | State::TimeWait => {}
        }

        // There might be more than one reason to send a packet. E.g. the keep-alive timer
        // has expired, and we also have data in transmit buffer. Since any packet that occupies
        // sequence space will elicit an ACK, we only need to send an explicit packet if we
        // couldn't fill the sequence space with anything.
        let is_keep_alive;
        if self.timer.should_keep_alive(cx.now()) && repr.is_empty() {
            repr.seq_number = repr.seq_number - 1;
            repr.payload = b"\x00"; // RFC 1122 says we should do this
            is_keep_alive = true;
        } else {
            is_keep_alive = false;
        }

        // Pure ACKs (including window updates during a PMTU retry) use
        // SND.NXT, not a cursor into the data being retransmitted. Probes
        // deliberately use an old sequence and retain their separate rule.
        if !is_keep_alive && repr.is_empty() && repr.control == TcpControl::None {
            repr.seq_number = self.acceptable_ack_sequence();
        }

        // Trace a summary of what will be sent.
        if is_keep_alive {
            tcp_trace!("sending a keep-alive");
        } else if !repr.payload.is_empty() {
            tcp_trace!(
                "tx buffer: sending {} octets at offset {}",
                repr.payload.len(),
                self.remote_last_seq - self.local_seq_no
            );
        }
        if repr.control != TcpControl::None || repr.payload.is_empty() {
            let flags = match (repr.control, repr.ack_number) {
                (TcpControl::Syn, None) => "SYN",
                (TcpControl::Syn, Some(_)) => "SYN|ACK",
                (TcpControl::Fin, Some(_)) => "FIN|ACK",
                (TcpControl::Rst, Some(_)) => "RST|ACK",
                (TcpControl::Psh, Some(_)) => "PSH|ACK",
                (TcpControl::None, Some(_)) => "ACK",
                _ => "<unreachable>",
            };
            tcp_trace!("sending {}", flags);
        }

        if repr.control == TcpControl::Syn {
            // Fill the MSS option. See RFC 6691 for an explanation of this calculation.
            let max_segment_size = ip_mtu.saturating_sub(ip_repr.header_len() + TCP_HEADER_LEN);
            repr.max_seg_size = Some(max_segment_size as u16);
        }

        if let Some(end) = self.tx_urgent_end {
            if repr.seq_number < end {
                let distance = end - repr.seq_number;
                repr.urgent_at = if distance <= u16::MAX as usize {
                    Some(distance as u16)
                } else if repr.seq_number + u16::MAX as usize > self.send_sequence_end() {
                    Some(u16::MAX)
                } else {
                    None
                };
            }
        }

        // Actually send the packet. If this succeeds, it means the packet is in
        // the device buffer, and its transmission is imminent. If not, we might have
        // a number of problems, e.g. we need neighbor discovery.
        //
        // Bailing out if the packet isn't placed in the device buffer allows us
        // to not waste time waiting for the retransmit timer on packets that we know
        // for sure will not be successfully transmitted.
        ip_repr.set_payload_len(repr.buffer_len());
        let packet_header_len = ip_repr.header_len() + repr.header_len();
        let admission = match emit(cx, (ip_repr, repr)) {
            Ok(admission) => admission,
            Err(error) => {
                if self.pmtu_retransmitting || self.mtu_retry_hint.is_some() {
                    let delay_ms = (1u64 << self.mtu_retry_failures.min(10)).min(1000);
                    self.mtu_retry_at = Some(cx.now() + Duration::from_millis(delay_ms));
                    self.mtu_retry_failures = self.mtu_retry_failures.saturating_add(1);
                }
                return Err(error);
            }
        };
        if let TcpPacketAdmission::MtuRetry(mtu) = admission {
            if mtu > packet_header_len && mtu < ip_mtu {
                self.mtu_retry_hint = Some((generation, mtu));
                self.output_ip_mtu = Some(mtu);
                self.mtu_retry_at = None;
                self.mtu_retry_failures = 0;
            } else {
                // A repeated or unusable hint cannot produce progress now.
                // Back off independently of congestion/RTO and policy denial.
                let delay_ms = (1u64 << self.mtu_retry_failures.min(10)).min(1000);
                self.mtu_retry_at = Some(cx.now() + Duration::from_millis(delay_ms));
                self.mtu_retry_failures = self.mtu_retry_failures.saturating_add(1);
            }
            return Ok(TcpDispatchOutcome::MtuRetry);
        }
        if admission == TcpPacketAdmission::PolicyDropped
            && repr.segment_len() > 0
            && repr.control != TcpControl::Syn
            && !is_keep_alive
        {
            // An unsent data/FIN segment stays in the transmit buffer. The
            // integration owns the retry deadline; returning success here
            // would advance remote_last_seq and hide the packet from it.
            return Ok(TcpDispatchOutcome::PolicyDeferred);
        }

        // We've sent something, whether useful data or a keep-alive packet, so rewind
        // the keep-alive timer.
        self.timer.rewind_keep_alive(cx.now(), self.keep_alive);

        // Reset delayed-ack timer
        match self.ack_delay_timer {
            AckDelayTimer::Idle => {}
            AckDelayTimer::Waiting(_) => {
                tcp_trace!("stop delayed ack timer")
            }
            AckDelayTimer::Immediate => {
                tcp_trace!("stop delayed ack timer (was force-expired)")
            }
        }
        self.ack_delay_timer = AckDelayTimer::Idle;

        // Leave the rest of the state intact if sending a keep-alive packet, since those
        // carry a fake segment.
        if is_keep_alive {
            return Ok(TcpDispatchOutcome::Done);
        }

        // We've sent a packet successfully, so we can update the internal state now.
        if repr.segment_len() > 0 {
            self.remote_last_seq = repr.seq_number + repr.segment_len();
        }
        if admission == TcpPacketAdmission::Admitted && repr.segment_len() > 0 {
            if self
                .admitted_seq_end
                .map_or(true, |end| self.remote_last_seq > end)
            {
                self.admitted_seq_end = Some(self.remote_last_seq);
            }
            self.mtu_retry_at = None;
            self.mtu_retry_failures = 0;
            if self
                .admitted_seq_end
                .is_some_and(|end| self.remote_last_seq >= end)
            {
                self.pmtu_retransmitting = false;
            }
        }
        self.remote_last_ack = repr.ack_number;
        self.remote_last_win = Self::advertised_window_bytes(&repr, self.remote_win_shift);

        if repr.segment_len() > 0 {
            self.rtte
                .on_send(cx.now(), repr.seq_number + repr.segment_len());
            self.congestion_controller
                .inner_mut()
                .post_transmit(cx.now(), repr.segment_len());
        }

        if !self.seq_to_transmit(cx) && repr.segment_len() > 0 {
            // If we've transmitted all data we could (and there was something at all,
            // data or flag, to transmit, not just an ACK), wind up the retransmit timer.
            self.timer
                .set_for_retransmit(cx.now(), self.rtte.retransmission_timeout());
        }

        if self.state == State::Closed {
            // When aborting a connection, forget about it after sending a single RST packet.
            self.tuple = None;
            #[cfg(feature = "async")]
            {
                // Wake tx now so that async users can wait for the RST to be sent
                self.tx_waker.wake();
            }
        }

        Ok(TcpDispatchOutcome::Done)
    }

    #[allow(clippy::if_same_then_else)]
    pub fn poll_at(&self, cx: &mut Context) -> PollAt {
        if let Some(tw) = &self.time_wait {
            if !self.ack_to_transmit() {
                return PollAt::Time(tw.expires);
            }
        }
        // The logic here mirrors the beginning of dispatch() closely.
        if self.tuple.is_none() {
            // No one to talk to, nothing to transmit.
            PollAt::Ingress
        } else if self.remote_last_ts.is_none() {
            // Socket stopped being quiet recently, we need to acquire a timestamp.
            PollAt::Now
        } else if self.state == State::Closed && self.mtu_retry_at.is_none() {
            // Socket was aborted, we have an RST packet to transmit.
            PollAt::Now
        } else if let Some(at) = self.mtu_retry_at {
            let timeout = match (self.remote_last_ts, self.timeout) {
                (Some(last), Some(timeout)) => PollAt::Time(last + timeout),
                _ => PollAt::Ingress,
            };
            *[PollAt::Time(at), self.timer.poll_at(), timeout]
                .iter()
                .min()
                .unwrap()
        } else if self.seq_to_transmit(cx)
            && self.policy_retry_at.is_none()
            && self.mtu_retry_at.is_none()
        {
            // We have a data or flag packet to transmit.
            PollAt::Now
        } else if self.window_to_update() {
            // The receive window has been raised significantly.
            PollAt::Now
        } else {
            let want_ack = self.ack_to_transmit();

            let delayed_ack_poll_at = match (want_ack, self.ack_delay_timer) {
                (false, _) => PollAt::Ingress,
                (true, AckDelayTimer::Idle) => PollAt::Now,
                (true, AckDelayTimer::Waiting(t)) => PollAt::Time(t),
                (true, AckDelayTimer::Immediate) => PollAt::Now,
            };

            let timeout_poll_at = match (self.remote_last_ts, self.timeout) {
                // If we're transmitting or retransmitting data, we need to poll at the moment
                // when the timeout would expire.
                (Some(remote_last_ts), Some(timeout)) => PollAt::Time(remote_last_ts + timeout),
                // Otherwise we have no timeout.
                (_, _) => PollAt::Ingress,
            };

            // We wait for the earliest of our timers to fire.
            let policy_poll_at = self.policy_retry_at.map_or(PollAt::Ingress, |at| {
                if self.seq_to_transmit(cx) {
                    PollAt::Time(at)
                } else {
                    PollAt::Ingress
                }
            });
            let mtu_poll_at = self.mtu_retry_at.map_or(PollAt::Ingress, PollAt::Time);
            *[
                self.timer.poll_at(),
                timeout_poll_at,
                delayed_ack_poll_at,
                policy_poll_at,
                mtu_poll_at,
            ]
            .iter()
            .min()
            .unwrap_or(&PollAt::Ingress)
        }
    }
}

impl<'a> fmt::Write for Socket<'a> {
    fn write_str(&mut self, slice: &str) -> fmt::Result {
        let slice = slice.as_bytes();
        if self.send_slice(slice) == Ok(slice.len()) {
            Ok(())
        } else {
            Err(fmt::Error)
        }
    }
}

// TODO: TCP should work for all features. For now, we only test with the IP feature. We could do
// it for other features as well with rstest, however, this means we have to modify a lot of the
// tests in here, which I didn't had the time for at the moment.
#[cfg(all(test, feature = "medium-ip"))]
mod test {
    use super::*;
    use crate::wire::IpRepr;
    use std::ops::{Deref, DerefMut};
    use std::vec::Vec;

    // =========================================================================================//
    // Constants
    // =========================================================================================//

    const LOCAL_PORT: u16 = 80;
    const REMOTE_PORT: u16 = 49500;
    const LISTEN_END: IpListenEndpoint = IpListenEndpoint {
        addr: None,
        port: LOCAL_PORT,
    };
    const TUPLE: Tuple = Tuple {
        local: LOCAL_END,
        remote: REMOTE_END,
    };
    const LOCAL_SEQ: TcpSeqNumber = TcpSeqNumber(10000);
    const REMOTE_SEQ: TcpSeqNumber = TcpSeqNumber(-10001);

    cfg_if::cfg_if! {
        if #[cfg(feature = "proto-ipv4")] {
            use crate::wire::Ipv4Address as IpvXAddress;
            use crate::wire::Ipv4Repr as IpvXRepr;
            use IpRepr::Ipv4 as IpReprIpvX;

            const LOCAL_ADDR: IpvXAddress = IpvXAddress::new(192, 168, 1, 1);
            const REMOTE_ADDR: IpvXAddress = IpvXAddress::new(192, 168, 1, 2);
            const OTHER_ADDR: IpvXAddress = IpvXAddress::new(192, 168, 1, 3);

            const BASE_MSS: u16 = 1460;

            const LOCAL_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv4(LOCAL_ADDR),
                port: LOCAL_PORT,
            };
            const REMOTE_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv4(REMOTE_ADDR),
                port: REMOTE_PORT,
            };
        } else {
            use crate::wire::Ipv6Address as IpvXAddress;
            use crate::wire::Ipv6Repr as IpvXRepr;
            use IpRepr::Ipv6 as IpReprIpvX;

            const LOCAL_ADDR: IpvXAddress = IpvXAddress::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
            const REMOTE_ADDR: IpvXAddress = IpvXAddress::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
            const OTHER_ADDR: IpvXAddress = IpvXAddress::new(0xfe80, 0, 0, 0, 0, 0, 0, 3);

            const BASE_MSS: u16 = 1440;

            const LOCAL_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv6(LOCAL_ADDR),
                port: LOCAL_PORT,
            };
            const REMOTE_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv6(REMOTE_ADDR),
                port: REMOTE_PORT,
            };
        }
    }

    const SEND_IP_TEMPL: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: LOCAL_ADDR,
        dst_addr: REMOTE_ADDR,
        next_header: IpProtocol::Tcp,
        payload_len: 20,
        hop_limit: 64,
    });
    const SEND_TEMPL: TcpRepr<'static> = TcpRepr {
        src_port: REMOTE_PORT,
        dst_port: LOCAL_PORT,
        control: TcpControl::None,
        seq_number: TcpSeqNumber(0),
        ack_number: Some(TcpSeqNumber(0)),
        urgent_at: None,
        window_len: 256,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };
    const _RECV_IP_TEMPL: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: LOCAL_ADDR,
        dst_addr: REMOTE_ADDR,
        next_header: IpProtocol::Tcp,
        payload_len: 20,
        hop_limit: 64,
    });
    const RECV_TEMPL: TcpRepr<'static> = TcpRepr {
        src_port: LOCAL_PORT,
        dst_port: REMOTE_PORT,
        control: TcpControl::None,
        seq_number: TcpSeqNumber(0),
        ack_number: Some(TcpSeqNumber(0)),
        urgent_at: None,
        window_len: 64,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };

    // =========================================================================================//
    // Helper functions
    // =========================================================================================//

    struct TestSocket {
        socket: Socket<'static>,
        cx: Context,
    }

    impl Deref for TestSocket {
        type Target = Socket<'static>;
        fn deref(&self) -> &Self::Target {
            &self.socket
        }
    }

    impl DerefMut for TestSocket {
        fn deref_mut(&mut self) -> &mut Self::Target {
            &mut self.socket
        }
    }

    fn send(
        socket: &mut TestSocket,
        timestamp: Instant,
        repr: &TcpRepr,
    ) -> Option<TcpRepr<'static>> {
        socket.cx.set_now(timestamp);

        let ip_repr = IpReprIpvX(IpvXRepr {
            src_addr: REMOTE_ADDR,
            dst_addr: LOCAL_ADDR,
            next_header: IpProtocol::Tcp,
            payload_len: repr.buffer_len(),
            hop_limit: 64,
        });
        net_trace!("send: {}", repr);

        assert!(socket.socket.accepts(&mut socket.cx, &ip_repr, repr));

        match socket.socket.process(&mut socket.cx, &ip_repr, repr) {
            Some((_ip_repr, repr)) => {
                net_trace!("recv: {}", repr);
                Some(repr)
            }
            None => None,
        }
    }

    fn recv<F>(socket: &mut TestSocket, timestamp: Instant, mut f: F)
    where
        F: FnMut(Result<TcpRepr, ()>),
    {
        socket.cx.set_now(timestamp);

        let mut sent = 0;
        let result = socket
            .socket
            .dispatch(&mut socket.cx, |_, (ip_repr, tcp_repr)| {
                assert_eq!(ip_repr.next_header(), IpProtocol::Tcp);
                assert_eq!(ip_repr.src_addr(), LOCAL_ADDR.into());
                assert_eq!(ip_repr.dst_addr(), REMOTE_ADDR.into());
                assert_eq!(ip_repr.payload_len(), tcp_repr.buffer_len());

                net_trace!("recv: {}", tcp_repr);
                sent += 1;
                Ok(f(Ok(tcp_repr)))
            });
        match result {
            Ok(()) => assert_eq!(sent, 1, "Exactly one packet should be sent"),
            Err(e) => f(Err(e)),
        }
    }

    fn recv_nothing(socket: &mut TestSocket, timestamp: Instant) {
        socket.cx.set_now(timestamp);

        let result: Result<(), ()> = socket
            .socket
            .dispatch(&mut socket.cx, |_, (_ip_repr, _tcp_repr)| {
                panic!("Should not send a packet")
            });

        assert_eq!(result, Ok(()))
    }

    macro_rules! send {
        ($socket:ident, $repr:expr) =>
            (send!($socket, time 0, $repr));
        ($socket:ident, $repr:expr, $result:expr) =>
            (send!($socket, time 0, $repr, $result));
        ($socket:ident, time $time:expr, $repr:expr) =>
            (send!($socket, time $time, $repr, None));
        ($socket:ident, time $time:expr, $repr:expr, $result:expr) =>
            (assert_eq!(send(&mut $socket, Instant::from_millis($time), &$repr), $result));
    }

    macro_rules! recv {
        ($socket:ident, [$( $repr:expr ),*]) => ({
            $( recv!($socket, Ok($repr)); )*
            recv_nothing!($socket)
        });
        ($socket:ident, $result:expr) =>
            (recv!($socket, time 0, $result));
        ($socket:ident, time $time:expr, $result:expr) =>
            (recv(&mut $socket, Instant::from_millis($time), |result| {
                // Most of the time we don't care about the PSH flag.
                let result = result.map(|mut repr| {
                    repr.control = repr.control.quash_psh();
                    repr
                });
                assert_eq!(result, $result)
            }));
        ($socket:ident, time $time:expr, $result:expr, exact) =>
            (recv(&mut $socket, Instant::from_millis($time), |repr| assert_eq!(repr, $result)));
    }

    macro_rules! recv_nothing {
        ($socket:ident) => (recv_nothing!($socket, time 0));
        ($socket:ident, time $time:expr) => (recv_nothing(&mut $socket, Instant::from_millis($time)));
    }

    macro_rules! sanity {
        ($socket1:expr, $socket2:expr) => {{
            let (s1, s2) = ($socket1, $socket2);
            assert_eq!(s1.state, s2.state, "state");
            assert_eq!(s1.tuple, s2.tuple, "tuple");
            assert_eq!(s1.local_seq_no, s2.local_seq_no, "local_seq_no");
            assert_eq!(s1.remote_seq_no, s2.remote_seq_no, "remote_seq_no");
            assert_eq!(s1.remote_last_seq, s2.remote_last_seq, "remote_last_seq");
            assert_eq!(s1.remote_last_ack, s2.remote_last_ack, "remote_last_ack");
            assert_eq!(s1.remote_last_win, s2.remote_last_win, "remote_last_win");
            assert_eq!(s1.remote_win_len, s2.remote_win_len, "remote_win_len");
            assert_eq!(s1.timer, s2.timer, "timer");
        }};
    }

    fn socket() -> TestSocket {
        socket_with_buffer_sizes(64, 64)
    }

    fn socket_with_buffer_sizes(tx_len: usize, rx_len: usize) -> TestSocket {
        let (iface, _, _) = crate::tests::setup(crate::phy::Medium::Ip);

        let rx_buffer = SocketBuffer::new(vec![0; rx_len]);
        let tx_buffer = SocketBuffer::new(vec![0; tx_len]);
        let mut socket = Socket::new(rx_buffer, tx_buffer);
        socket.set_ack_delay(None);
        TestSocket {
            socket,
            cx: iface.inner,
        }
    }

    fn socket_syn_received_with_buffer_sizes(tx_len: usize, rx_len: usize) -> TestSocket {
        let mut s = socket_with_buffer_sizes(tx_len, rx_len);
        s.state = State::SynReceived;
        s.tuple = Some(TUPLE);
        s.local_seq_no = LOCAL_SEQ;
        s.remote_seq_no = REMOTE_SEQ + 1;
        s.remote_last_seq = LOCAL_SEQ;
        s.remote_win_len = 256;
        s
    }

    fn socket_syn_received() -> TestSocket {
        socket_syn_received_with_buffer_sizes(64, 64)
    }

    fn socket_syn_sent_with_buffer_sizes(tx_len: usize, rx_len: usize) -> TestSocket {
        let mut s = socket_with_buffer_sizes(tx_len, rx_len);
        s.state = State::SynSent;
        s.tuple = Some(TUPLE);
        s.local_seq_no = LOCAL_SEQ;
        s.remote_last_seq = LOCAL_SEQ;
        s
    }

    fn socket_syn_sent() -> TestSocket {
        socket_syn_sent_with_buffer_sizes(64, 64)
    }

    fn socket_established_with_buffer_sizes(tx_len: usize, rx_len: usize) -> TestSocket {
        let mut s = socket_syn_received_with_buffer_sizes(tx_len, rx_len);
        s.state = State::Established;
        s.local_seq_no = LOCAL_SEQ + 1;
        s.remote_last_seq = LOCAL_SEQ + 1;
        s.remote_last_ack = Some(REMOTE_SEQ + 1);
        s.remote_last_win = usize::from(s.scaled_window()) << s.remote_win_shift;
        s
    }

    fn socket_established() -> TestSocket {
        socket_established_with_buffer_sizes(64, 64)
    }

    fn pmtu_dispatch(
        s: &mut TestSocket,
        mtu: usize,
        generation: u64,
        admission: TcpPacketAdmission,
    ) -> TcpDispatchOutcome {
        let TestSocket { socket, cx } = s;
        socket
            .dispatch_with_mtu_policy_generation(cx, mtu, generation, |_, _| Ok::<_, ()>(admission))
            .unwrap()
    }

    #[test]
    fn ack_sequence_uses_send_high_water_after_wrap_and_rewind() {
        for start in [LOCAL_SEQ + 1, TcpSeqNumber(i32::MAX - 8)] {
            let mut s = socket_established();
            s.local_seq_no = start;
            s.remote_last_seq = start;
            s.send_slice(&[1; 32]).unwrap();
            pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
            let end = start + 32;
            assert_eq!(s.send_sequence_end(), end);
            s.remote_last_seq = start + 8;
            let ip = IpRepr::new(TUPLE.remote.addr, TUPLE.local.addr, IpProtocol::Tcp, 20, 64);
            // A duplicate segment elicits an immediate ACK without waiting
            // for another egress pass. Its sequence must survive cursor rewind.
            let probe = TcpRepr {
                seq_number: s.remote_seq_no - 1,
                ack_number: Some(start),
                urgent_at: None,
                ..SEND_TEMPL
            };
            let TestSocket { socket, cx } = &mut s;
            let (_, ack) = socket.process(cx, &ip, &probe).unwrap();
            assert_eq!(ack.seq_number, end);
            assert_eq!(s.remote_last_seq, start + 8);
        }
    }

    #[test]
    fn ack_sequence_obeys_zero_shrunken_and_scaled_peer_window() {
        let mut s = socket_established();
        let start = s.local_seq_no;
        s.admitted_seq_end = Some(start + 32);
        s.remote_last_seq = start + 8;
        s.remote_win_len = 0;
        assert_eq!(s.acceptable_ack_sequence(), start);
        s.remote_win_len = 16;
        assert_eq!(s.acceptable_ack_sequence(), start + 16);
        s.remote_win_shift = 2;
        s.remote_win_len = 29;
        s.remote_win_scale = None;
        assert_eq!(s.acceptable_ack_sequence(), start + 29);
        // Peer and local scale differ: Linux's rounding allowance uses the
        // local advertised receive shift, not the peer's shift.
        s.remote_win_scale = Some(7);
        assert_eq!(s.acceptable_ack_sequence(), start + 32);
        s.remote_win_len = 28;
        assert_eq!(s.acceptable_ack_sequence(), start + 28);
        // A FIN also occupies sequence space, unlike an empty ACK.
        s.admitted_seq_end = Some(start + 33);
        s.remote_win_len = 256;
        assert_eq!(s.acceptable_ack_sequence(), start + 33);
        s.reset();
        assert_eq!(s.admitted_seq_end, None);
        assert_eq!(s.send_sequence_end(), TcpSeqNumber::default());
    }

    #[test]
    fn ack_only_dispatch_does_not_skip_pmtu_retransmission_cursor() {
        let mut s = socket_established();
        s.send_slice(&[1; 32]).unwrap();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        let end = s.admitted_seq_end.unwrap();
        assert!(s.schedule_pmtu_retransmit(1280));
        let cursor = s.remote_last_seq;
        let timer = s.timer;
        s.policy_retry_at = Some(s.cx.now() + Duration::from_secs(1));
        s.remote_last_ack = Some(s.remote_seq_no - 1);
        let mut emitted = false;
        let TestSocket { socket, cx } = &mut s;
        socket
            .dispatch_with_mtu_policy(cx, 1280, |_, (_, tcp)| {
                emitted = true;
                assert!(tcp.is_empty());
                assert_eq!(tcp.seq_number, end);
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        assert!(emitted);
        assert_eq!(s.remote_last_seq, cursor);
        assert_eq!(s.admitted_seq_end, Some(end));
        assert_eq!(s.timer, timer);
        assert_eq!(s.tx_buffer.len(), 32);
    }

    #[test]
    fn partial_retransmission_ack_keeps_timer_and_duplicate_detection() {
        let mut s = socket_established();
        s.send_slice(&[1; 32]).unwrap();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        let start = s.local_seq_no;
        let timer = s.timer;
        s.remote_last_seq = start + 8;
        let ip = IpRepr::new(TUPLE.remote.addr, TUPLE.local.addr, IpProtocol::Tcp, 20, 64);
        let ack = TcpRepr {
            seq_number: s.remote_seq_no,
            ack_number: Some(start + 8),
            urgent_at: None,
            window_len: s.remote_win_len as u16,
            ..SEND_TEMPL
        };
        let TestSocket { socket, cx } = &mut s;
        assert!(socket.process(cx, &ip, &ack).is_none());
        assert_eq!(s.timer, timer);
        assert_eq!(s.tx_buffer.len(), 24);
        // The cursor equals SND.UNA, but the admitted high water still has
        // outstanding data: duplicate ACKs must still trigger fast recovery.
        for _ in 0..3 {
            let TestSocket { socket, cx } = &mut s;
            socket.process(cx, &ip, &ack);
        }
        assert_eq!(s.timer, Timer::FastRetransmit);
    }

    #[test]
    fn rto_ack_sequence_keeps_high_water_while_data_cursor_rewinds() {
        let mut s = socket_established();
        s.send_slice(&[1; 32]).unwrap();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        let start = s.local_seq_no;
        let end = s.admitted_seq_end.unwrap();
        let Timer::Retransmit { expires_at, .. } = s.timer else {
            panic!("initial transmission must arm the RTO");
        };
        s.cx.set_now(expires_at + Duration::from_millis(1));
        s.remote_mss = 8;
        let mut emitted = false;
        let TestSocket { socket, cx } = &mut s;
        socket
            .dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                emitted = true;
                assert_eq!(tcp.seq_number, start);
                assert_eq!(tcp.payload.len(), 8);
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        assert!(emitted);
        assert_eq!(s.remote_last_seq, start + 8);
        assert_eq!(s.send_sequence_end(), end);
        assert_eq!(s.acceptable_ack_sequence(), end);
    }

    #[test]
    fn ack_high_water_does_not_suppress_syn_retransmission() {
        for mut s in [socket_syn_sent(), socket_syn_received()] {
            pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
            assert_eq!(s.admitted_seq_end, Some(LOCAL_SEQ + 1));
            let Timer::Retransmit { expires_at, .. } = s.timer else {
                panic!("initial SYN must arm the RTO");
            };
            s.cx.set_now(expires_at + Duration::from_millis(1));
            let mut emitted = false;
            let TestSocket { socket, cx } = &mut s;
            socket
                .dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                    emitted = true;
                    assert_eq!(tcp.control, TcpControl::Syn);
                    assert_eq!(tcp.seq_number, LOCAL_SEQ);
                    Ok::<_, ()>(TcpPacketAdmission::Admitted)
                })
                .unwrap();
            assert!(emitted);
            assert_eq!(s.remote_last_seq, LOCAL_SEQ + 1);
            assert_eq!(s.admitted_seq_end, Some(LOCAL_SEQ + 1));
        }
    }

    #[test]
    fn pmtu_quote_high_water_survives_wrap_and_rewind() {
        let mut s = socket_established();
        let start = TcpSeqNumber(i32::MAX - 8);
        s.local_seq_no = start;
        s.remote_last_seq = start;
        s.send_slice(&[1; 32]).unwrap();
        assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, start, 0));
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        let end = start + 32;
        s.remote_last_seq = start; // RTO/PMTU retransmit cursor rewind.
        assert!(s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, start, 0));
        assert!(s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, end, 0));
        assert!(s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, start + 16, 0));
        assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, start - 1, 0));
        assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, end + 1, 0));
        s.local_seq_no = start + 16; // Cumulative ACK advances snd_una.
        assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, start, 0));
        let mut wrong = TUPLE.remote;
        wrong.port += 1;
        assert!(!s.accepts_pmtu_quote(TUPLE.local, wrong, end, 0));
        #[cfg(feature = "packetmeta-id")]
        {
            s.set_bound_device(NonZeroU32::new(7));
            assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, end, 8));
            assert!(s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, end, 7));
        }
        s.state = State::TimeWait;
        assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, end, 7));
    }

    #[test]
    fn pmtu_retransmit_preserves_congestion_rto_and_zero_window_timer() {
        let mut s = socket_established_with_buffer_sizes(4096, 4096);
        #[cfg(feature = "socket-tcp-reno")]
        s.set_congestion_control(CongestionControl::Reno);
        s.remote_mss = 4000;
        s.remote_win_len = 4096;
        s.send_slice(&[1; 1460]).unwrap();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        let seq_end = s.admitted_seq_end;
        let timer = s.timer;
        let cwnd = s.cwnd();
        let rto = s.rto();
        let retransmits = s.retransmits();
        assert!(s.schedule_pmtu_retransmit(1280));
        assert_eq!(s.remote_last_seq, s.local_seq_no);
        assert_eq!(s.admitted_seq_end, seq_end);
        assert_eq!(s.cwnd(), cwnd);
        assert_eq!(s.rto(), rto);
        assert_eq!(s.retransmits(), retransmits);
        assert_eq!(s.timer, timer);
        assert!(s.rtte.timestamp.is_none());
        assert!(!s.schedule_pmtu_retransmit(1280));
        let start = s.local_seq_no;
        let TestSocket { socket, cx } = &mut s;
        socket
            .dispatch_with_mtu_policy(cx, 1280, |_, (ip, tcp)| {
                assert_eq!(tcp.seq_number, start);
                assert_eq!(ip.header_len() + tcp.buffer_len(), 1280);
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        assert_eq!(s.cwnd(), cwnd);
        assert_eq!(s.retransmits(), retransmits);
        assert_eq!(s.timer, timer);
        s.remote_win_len = 0;
        let TestSocket { socket, cx } = &mut s;
        assert_ne!(socket.poll_at(cx), PollAt::Now);
        socket
            .dispatch_with_mtu_policy(cx, 1280, |_, _| {
                panic!("zero window must retain the retransmit deadline");
                #[allow(unreachable_code)]
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        assert_eq!(s.timer, timer);
    }

    #[test]
    fn pmtu_mtu_retry_does_not_admit_and_repeated_hint_backs_off() {
        let mut s = socket_established_with_buffer_sizes(4096, 4096);
        s.remote_mss = 4000;
        s.remote_win_len = 4096;
        s.send_slice(&[1; 1460]).unwrap();
        let start = s.remote_last_seq;
        let rto = s.rto();
        let cwnd = s.cwnd();
        assert_eq!(
            pmtu_dispatch(&mut s, 1500, 3, TcpPacketAdmission::MtuRetry(1280)),
            TcpDispatchOutcome::MtuRetry
        );
        assert_eq!(s.remote_last_seq, start);
        assert_eq!(s.admitted_seq_end, None);
        assert!(s.rtte.timestamp.is_none());
        assert_eq!(s.rto(), rto);
        assert_eq!(s.cwnd(), cwnd);
        assert_eq!(s.output_ip_mtu, Some(1280));
        assert_eq!(
            pmtu_dispatch(&mut s, 1500, 3, TcpPacketAdmission::MtuRetry(1280)),
            TcpDispatchOutcome::MtuRetry
        );
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.poll_at(cx),
            PollAt::Time(cx.now() + Duration::from_millis(1))
        );
        socket
            .dispatch_with_mtu_policy_generation(cx, 1500, 3, |_, _| {
                panic!("same hint must not emit again before the deadline");
                #[allow(unreachable_code)]
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        // Replacing rules/routes discards the old local hint and its backoff.
        let TestSocket { socket, cx } = &mut s;
        let mut admitted_end = None;
        socket
            .dispatch_with_mtu_policy_generation(cx, 1500, 4, |_, (ip, tcp)| {
                assert_eq!(ip.header_len() + tcp.buffer_len(), 1500);
                // IPv6's larger IP header leaves less payload at the same MTU.
                // Admission must track the segment actually emitted, not all
                // 1460 queued bytes or an IPv4-specific MSS assumption.
                admitted_end = Some(tcp.seq_number + tcp.segment_len());
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        assert!(admitted_end.is_some());
        assert_eq!(s.admitted_seq_end, admitted_end);
    }

    #[test]
    fn pmtu_retransmit_exhaustion_retains_data_and_timer() {
        let mut s = socket_established();
        s.send_slice(&[1; 32]).unwrap();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        let timer = s.timer;
        let end = s.admitted_seq_end;
        assert!(s.schedule_pmtu_retransmit(1280));
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.dispatch_with_mtu_policy(cx, 1280, |_, _| { Err::<TcpPacketAdmission, _>(()) }),
            Err(())
        );
        assert_eq!(s.remote_last_seq, s.local_seq_no);
        assert_eq!(s.admitted_seq_end, end);
        assert_eq!(s.timer, timer);
        assert_eq!(s.tx_buffer.len(), 32);
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.poll_at(cx),
            PollAt::Time(cx.now() + Duration::from_millis(1))
        );
        cx.set_now(cx.now() + Duration::from_millis(1));
        pmtu_dispatch(&mut s, 1280, 0, TcpPacketAdmission::Admitted);
        assert_eq!(s.remote_last_seq, end.unwrap());
    }

    #[test]
    fn pmtu_abort_obeys_output_backoff_without_empty_polling() {
        let mut s = socket_established();
        s.send_slice(&[1; 32]).unwrap();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        assert!(s.schedule_pmtu_retransmit(1280));
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.dispatch_with_mtu_policy(cx, 1280, |_, _| Err::<TcpPacketAdmission, _>(())),
            Err(())
        );
        socket.abort();
        let deadline = cx.now() + Duration::from_millis(1);
        assert_eq!(socket.poll_at(cx), PollAt::Time(deadline));
        assert_eq!(
            socket.dispatch_with_mtu_policy(cx, 1280, |_, _| -> Result<TcpPacketAdmission, ()> {
                panic!("output before retry deadline")
            }),
            Ok(TcpDispatchOutcome::Done)
        );
        cx.set_now(deadline);
        socket
            .dispatch_with_mtu_policy(cx, 1280, |_, (_, repr)| {
                assert_eq!(repr.control, TcpControl::Rst);
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        assert_eq!(socket.poll_at(cx), PollAt::Ingress);
    }

    #[test]
    fn pmtu_policy_validation_and_feedback_gates() {
        let mut s = socket_established();
        assert_eq!(s.pmtu_discover(), 1);
        assert_eq!(s.set_pmtu_discover(6), Err(PmtuDiscoverError));
        for mode in 0..=5 {
            assert_eq!(s.set_pmtu_discover(mode), Ok(()));
            assert_eq!(s.pmtu_discover(), mode);
            s.output_ip_mtu = Some(1500);
            let ipv4 = match TUPLE.local.addr {
                #[cfg(feature = "proto-ipv4")]
                IpAddress::Ipv4(_) => true,
                #[cfg(feature = "proto-ipv6")]
                IpAddress::Ipv6(_) => false,
            };
            assert_eq!(
                s.schedule_pmtu_retransmit(1280),
                mode < 4 && (!ipv4 || mode != 0)
            );
        }
    }

    #[test]
    fn pmtu_syn_policy_drop_is_not_admitted_sequence() {
        let mut s = socket_syn_sent();
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::PolicyDropped);
        assert_eq!(s.admitted_seq_end, None);
        assert!(!s.accepts_pmtu_quote(TUPLE.local, TUPLE.remote, LOCAL_SEQ, 0));
        s.timer = Timer::FastRetransmit;
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        assert_eq!(s.admitted_seq_end, Some(LOCAL_SEQ + 1));
    }

    #[test]
    fn pmtu_fin_and_syn_mtu_retry_do_not_admit_prematurely() {
        let mut syn = socket_syn_sent();
        pmtu_dispatch(&mut syn, 1500, 0, TcpPacketAdmission::MtuRetry(1280));
        assert_eq!(syn.remote_last_seq, LOCAL_SEQ);
        assert_eq!(syn.admitted_seq_end, None);
        pmtu_dispatch(&mut syn, 1500, 0, TcpPacketAdmission::Admitted);
        assert_eq!(syn.admitted_seq_end, Some(LOCAL_SEQ + 1));
        let mut fin = socket_established();
        fin.close();
        let start = fin.local_seq_no;
        pmtu_dispatch(&mut fin, 1500, 0, TcpPacketAdmission::MtuRetry(1280));
        assert_eq!(fin.remote_last_seq, start);
        assert_eq!(fin.admitted_seq_end, None);
        pmtu_dispatch(&mut fin, 1500, 0, TcpPacketAdmission::Admitted);
        assert_eq!(fin.admitted_seq_end, Some(start + 1));
    }

    #[test]
    fn routed_mtu_accounts_for_timestamp_and_nagle() {
        let mut s = socket_established_with_buffer_sizes(4096, 4096);
        #[cfg(feature = "proto-ipv6")]
        {
            let tuple = s.tuple.as_mut().unwrap();
            tuple.local.addr = crate::wire::Ipv6Address::LOCALHOST.into();
            tuple.remote.addr = crate::wire::Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1).into();
        }
        s.tsval_generator = Some(|| 123);
        s.remote_mss = 4000;
        s.remote_win_len = 4096;
        s.send_slice(&[7; 3000]).unwrap();
        let mut emitted = 0;
        for _ in 0..2 {
            let TestSocket { socket, cx } = &mut s;
            socket
                .dispatch_with_mtu(cx, 1280, |_, (ip, tcp)| {
                    assert_eq!(ip.header_len() + tcp.buffer_len(), 1280);
                    assert_eq!(tcp.header_len(), 32);
                    emitted += tcp.payload.len();
                    Ok::<_, ()>(())
                })
                .unwrap();
            if emitted < 2000 {
                assert_eq!(socket.poll_at(cx), PollAt::Now);
            }
        }
        assert!(emitted > 2000);
        s.reset();
        assert_eq!(s.output_ip_mtu, None);
    }

    #[test]
    fn policy_drop_does_not_advance_unsent_tcp_data() {
        let mut s = socket_established();
        s.send_slice(b"abc").unwrap();
        let sequence = s.remote_last_seq;
        let TestSocket { socket, cx } = &mut s;
        let result = socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
            assert_eq!(tcp.payload, b"abc");
            Ok::<_, ()>(TcpPacketAdmission::PolicyDropped)
        });
        assert_eq!(result, Ok(TcpDispatchOutcome::PolicyDeferred));
        assert_eq!(s.remote_last_seq, sequence);
        assert_eq!(s.tx_buffer.len(), 3);
        let retry_at = s.policy_retry_at.unwrap();
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Time(retry_at));
        assert!(retry_at >= s.cx.now() + Duration::from_millis(POLICY_PROBE_MIN_MS));
        let TestSocket { socket, cx } = &mut s;
        let result: Result<TcpDispatchOutcome, ()> =
            socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                panic!("policy changes must not bypass the persist timer: {tcp:?}")
            });
        assert_eq!(result, Ok(TcpDispatchOutcome::Done));
        s.cx.set_now(retry_at);
        let TestSocket { socket, cx } = &mut s;
        let result = socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
            assert_eq!(tcp.payload, b"abc");
            Ok::<_, ()>(TcpPacketAdmission::Admitted)
        });
        assert_eq!(result, Ok(TcpDispatchOutcome::Done));
        assert_eq!(s.remote_last_seq, sequence + 3);
    }

    #[test]
    fn policy_denied_data_uses_persist_floor_and_backoff() {
        let mut s = socket_established();
        s.rtte.rtt = 5;
        s.rtte.deviation = 0;
        s.send_slice(b"abc").unwrap();
        let initial = s.cx.now();
        for (attempt, expected_delay_ms) in [(1, 200), (2, 400), (3, 800)] {
            let TestSocket { socket, cx } = &mut s;
            assert_eq!(
                socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                    assert_eq!(tcp.payload, b"abc");
                    Ok::<_, ()>(TcpPacketAdmission::PolicyDropped)
                }),
                Ok(TcpDispatchOutcome::PolicyDeferred)
            );
            let deadline = s.policy_retry_at.unwrap();
            assert_eq!(
                deadline,
                s.cx.now() + Duration::from_millis(expected_delay_ms)
            );
            assert_eq!(s.policy_retry_failures, attempt);
            s.cx.set_now(deadline);
        }
        assert!(s.cx.now() >= initial + Duration::from_millis(1400));
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                assert_eq!(tcp.payload, b"abc");
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            }),
            Ok(TcpDispatchOutcome::Done)
        );
        assert_eq!(s.policy_retry_at, None);
        assert_eq!(s.policy_retry_failures, 0);
    }

    #[test]
    fn policy_drop_of_syn_still_starts_retransmission() {
        let mut s = socket_syn_sent();
        let TestSocket { socket, cx } = &mut s;
        let result = socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
            assert_eq!(tcp.control, TcpControl::Syn);
            Ok::<_, ()>(TcpPacketAdmission::PolicyDropped)
        });
        assert_eq!(result, Ok(TcpDispatchOutcome::Done));
        assert_eq!(s.remote_last_seq, LOCAL_SEQ + 1);
        assert!(matches!(s.timer, Timer::Retransmit { .. }));
    }

    #[test]
    fn deferred_new_data_does_not_starve_older_retransmission() {
        let mut s = socket_established();
        s.set_nagle_enabled(false);
        s.send_slice(b"a").unwrap();
        let TestSocket { socket, cx } = &mut s;
        socket
            .dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                assert_eq!(tcp.payload, b"a");
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
        let Timer::Retransmit { expires_at, .. } = s.timer else {
            panic!("the first sent segment must start retransmission")
        };
        s.cx.set_now(expires_at - Duration::from_millis(1));
        s.send_slice(b"b").unwrap();
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                assert_eq!(tcp.payload, b"b");
                Ok::<_, ()>(TcpPacketAdmission::PolicyDropped)
            }),
            Ok(TcpDispatchOutcome::PolicyDeferred)
        );
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Time(expires_at));
        s.cx.set_now(expires_at);
        let TestSocket { socket, cx } = &mut s;
        socket
            .dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                assert_eq!(tcp.seq_number, LOCAL_SEQ + 1);
                assert!(tcp.payload.starts_with(b"a"));
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            })
            .unwrap();
    }

    #[test]
    fn deferred_data_does_not_suppress_pure_ack() {
        let mut s = socket_established();
        s.send_slice(b"data").unwrap();
        let TestSocket { socket, cx } = &mut s;
        socket
            .dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                assert_eq!(tcp.payload, b"data");
                Ok::<_, ()>(TcpPacketAdmission::PolicyDropped)
            })
            .unwrap();
        let old_send_head = s.remote_last_seq;
        s.remote_last_ack = Some(REMOTE_SEQ);
        s.ack_delay_timer = AckDelayTimer::Immediate;
        let TestSocket { socket, cx } = &mut s;
        assert_eq!(
            socket.dispatch_with_mtu_policy(cx, 1500, |_, (_, tcp)| {
                assert!(tcp.payload.is_empty());
                assert_eq!(tcp.control, TcpControl::None);
                Ok::<_, ()>(TcpPacketAdmission::Admitted)
            }),
            Ok(TcpDispatchOutcome::Done)
        );
        assert_eq!(s.remote_last_seq, old_send_head);
        assert!(s.policy_retry_at.is_some());
    }

    fn socket_fin_wait_1() -> TestSocket {
        let mut s = socket_established();
        s.state = State::FinWait1;
        s
    }

    fn socket_fin_wait_2() -> TestSocket {
        let mut s = socket_fin_wait_1();
        s.state = State::FinWait2;
        s.local_seq_no = LOCAL_SEQ + 1 + 1;
        s.remote_last_seq = LOCAL_SEQ + 1 + 1;
        s
    }

    fn socket_closing() -> TestSocket {
        let mut s = socket_fin_wait_1();
        s.state = State::Closing;
        s.remote_last_seq = LOCAL_SEQ + 1 + 1;
        s.remote_seq_no = REMOTE_SEQ + 1 + 1;
        s
    }

    fn socket_time_wait(from_closing: bool) -> TestSocket {
        let mut s = socket_fin_wait_2();
        s.state = State::TimeWait;
        s.remote_seq_no = REMOTE_SEQ + 1 + 1;
        if from_closing {
            s.remote_last_ack = Some(REMOTE_SEQ + 1 + 1);
        }
        s.timer = Timer::Close {
            expires_at: Instant::from_secs(1) + CLOSE_DELAY,
        };
        s
    }

    fn socket_close_wait() -> TestSocket {
        let mut s = socket_established();
        s.state = State::CloseWait;
        s.remote_seq_no = REMOTE_SEQ + 1 + 1;
        s.remote_last_ack = Some(REMOTE_SEQ + 1 + 1);
        s
    }

    fn socket_last_ack() -> TestSocket {
        let mut s = socket_close_wait();
        s.state = State::LastAck;
        s
    }

    fn socket_recved() -> TestSocket {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            }]
        );
        s
    }

    // =========================================================================================//
    // Tests for the CLOSED state.
    // =========================================================================================//
    #[test]
    fn test_closed_reject() {
        let mut s = socket();
        assert_eq!(s.state, State::Closed);

        let tcp_repr = TcpRepr {
            control: TcpControl::Syn,
            ..SEND_TEMPL
        };
        assert!(!s.socket.accepts(&mut s.cx, &SEND_IP_TEMPL, &tcp_repr));
    }

    #[test]
    fn test_closed_reject_after_listen() {
        let mut s = socket();
        s.listen(LOCAL_END).unwrap();
        s.close();

        let tcp_repr = TcpRepr {
            control: TcpControl::Syn,
            ..SEND_TEMPL
        };
        assert!(!s.socket.accepts(&mut s.cx, &SEND_IP_TEMPL, &tcp_repr));
    }

    #[test]
    fn test_closed_close() {
        let mut s = socket();
        s.close();
        assert_eq!(s.state, State::Closed);
    }

    // =========================================================================================//
    // Tests for the LISTEN state.
    // =========================================================================================//
    fn socket_listen() -> TestSocket {
        let mut s = socket();
        s.state = State::Listen;
        s.listen_endpoint = LISTEN_END;
        s
    }

    #[test]
    fn test_listen_sack_option() {
        let mut s = socket_listen();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                sack_permitted: false,
                ..SEND_TEMPL
            }
        );
        assert!(!s.remote_has_sack);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );

        let mut s = socket_listen();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                sack_permitted: true,
                ..SEND_TEMPL
            }
        );
        assert!(s.remote_has_sack);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_listen_syn_win_scale_buffers() {
        for (buffer_size, shift_amt) in &[
            (64, 0),
            (128, 0),
            (1024, 0),
            (65535, 0),
            (65536, 1),
            (65537, 1),
            (131071, 1),
            (131072, 2),
            (524287, 3),
            (524288, 4),
            (655350, 4),
            (1048576, 5),
        ] {
            let mut s = socket_with_buffer_sizes(64, *buffer_size);
            s.state = State::Listen;
            s.listen_endpoint = LISTEN_END;
            assert_eq!(s.remote_win_shift, *shift_amt);
            send!(
                s,
                TcpRepr {
                    control: TcpControl::Syn,
                    seq_number: REMOTE_SEQ,
                    ack_number: None,
                    urgent_at: None,
                    window_scale: Some(0),
                    ..SEND_TEMPL
                }
            );
            assert_eq!(s.remote_win_shift, *shift_amt);
            recv!(
                s,
                [TcpRepr {
                    control: TcpControl::Syn,
                    seq_number: LOCAL_SEQ,
                    ack_number: Some(REMOTE_SEQ + 1),
                    urgent_at: None,
                    max_seg_size: Some(BASE_MSS),
                    window_scale: Some(*shift_amt),
                    window_len: u16::try_from(*buffer_size).unwrap_or(u16::MAX),
                    ..RECV_TEMPL
                }]
            );
        }
    }

    #[test]
    fn test_listen_sanity() {
        let mut s = socket();
        s.listen(LOCAL_PORT).unwrap();
        sanity!(s, socket_listen());
    }

    #[test]
    #[cfg(all(feature = "proto-ipv4", feature = "proto-ipv6"))]
    fn test_listen_ip_version_matching() {
        use crate::wire::{Ipv4Address, Ipv4Repr, Ipv6Address, Ipv6Repr};

        let packets = [
            IpRepr::Ipv4(Ipv4Repr {
                src_addr: Ipv4Address::new(192, 0, 2, 1),
                dst_addr: Ipv4Address::new(192, 0, 2, 2),
                next_header: IpProtocol::Tcp,
                payload_len: 20,
                hop_limit: 64,
            }),
            IpRepr::Ipv6(Ipv6Repr {
                src_addr: Ipv6Address::LOCALHOST,
                dst_addr: Ipv6Address::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 2),
                next_header: IpProtocol::Tcp,
                payload_len: 20,
                hop_limit: 64,
            }),
        ];
        let syn = TcpRepr {
            control: TcpControl::Syn,
            ack_number: None,
            urgent_at: None,
            ..SEND_TEMPL
        };
        for constraint in [None, Some(IpVersion::Ipv4), Some(IpVersion::Ipv6)] {
            let mut s = socket();
            assert_eq!(s.listen_ip_version(), None);
            s.set_listen_ip_version(constraint);
            s.listen(LOCAL_PORT).unwrap();
            for packet in &packets {
                let expected = constraint.map_or(true, |v| v == packet.version());
                assert_eq!(s.socket.accepts(&mut s.cx, packet, &syn), expected);
                // Reopening the socket must not clear its configured constraint.
                s.abort();
                s.listen(LOCAL_PORT).unwrap();
                assert_eq!(s.listen_ip_version(), constraint);
                assert_eq!(s.socket.accepts(&mut s.cx, packet, &syn), expected);
            }
            for local in &packets {
                s.abort();
                s.listen((local.dst_addr(), LOCAL_PORT)).unwrap();
                for packet in &packets {
                    let expected = packet.dst_addr() == local.dst_addr()
                        && constraint.map_or(true, |v| v == packet.version());
                    assert_eq!(s.socket.accepts(&mut s.cx, packet, &syn), expected);
                }
            }
        }
    }

    #[test]
    #[cfg(all(feature = "proto-ipv4", feature = "proto-ipv6"))]
    fn test_listen_ip_version_does_not_filter_existing_tuple() {
        let mut s = socket_established();
        let packet = IpReprIpvX(IpvXRepr {
            src_addr: REMOTE_ADDR,
            dst_addr: LOCAL_ADDR,
            next_header: IpProtocol::Tcp,
            payload_len: 20,
            hop_limit: 64,
        });
        let other = match packet.version() {
            IpVersion::Ipv4 => IpVersion::Ipv6,
            IpVersion::Ipv6 => IpVersion::Ipv4,
        };
        s.set_listen_ip_version(Some(other));
        assert!(s.socket.accepts(&mut s.cx, &packet, &SEND_TEMPL));
    }

    #[test]
    fn test_listen_validation() {
        let mut s = socket();
        assert_eq!(s.listen(0), Err(ListenError::Unaddressable));
    }

    #[test]
    fn test_listen_twice() {
        let mut s = socket();
        assert_eq!(s.listen(80), Ok(()));
        // multiple calls to listen are okay if its the same local endpoint and the state is still in listening
        assert_eq!(s.listen(80), Ok(()));
        s.set_state(State::SynReceived); // state change, simulate incoming connection
        assert_eq!(s.listen(80), Err(ListenError::InvalidState));
    }

    #[test]
    fn test_listen_syn() {
        let mut s = socket_listen();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        sanity!(s, socket_syn_received());
    }

    #[test]
    fn test_passive_pmtu_policy_validation_is_atomic_and_pending_is_stable() {
        let mut s = socket_listen();
        s.set_pmtu_discover(5).unwrap();
        s.set_listen_pmtu_policies([1, 2]).unwrap();
        assert_eq!(s.set_listen_pmtu_policies([3, 6]), Err(PmtuDiscoverError));
        assert_eq!(s.listen_pmtu_policies, Some([1, 2]));
        let invalid = TcpRepr {
            control: TcpControl::Syn,
            seq_number: REMOTE_SEQ,
            ack_number: None,
            urgent_at: None,
            max_seg_size: Some(0),
            ..SEND_TEMPL
        };
        send(&mut s, Instant::ZERO, &invalid);
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.pmtu_discover(), 5);
        let valid = TcpRepr {
            max_seg_size: Some(BASE_MSS),
            ..invalid
        };
        send(&mut s, Instant::ZERO, &valid);
        assert_eq!(s.state, State::SynReceived);
        let expected = match LOCAL_END.addr {
            #[cfg(feature = "proto-ipv4")]
            IpAddress::Ipv4(_) => 1,
            #[cfg(feature = "proto-ipv6")]
            IpAddress::Ipv6(_) => 2,
        };
        assert_eq!(s.pmtu_discover(), expected);
        s.set_listen_pmtu_policies([3, 4]).unwrap();
        assert_eq!(s.pmtu_discover(), expected);
    }

    #[test]
    #[cfg(all(feature = "proto-ipv4", feature = "proto-ipv6"))]
    fn test_dual_stack_passive_pmtu_uses_actual_matching_syn_family() {
        for ipv6 in [false, true] {
            let mut s = socket_listen();
            s.listen_endpoint.addr = None;
            s.set_listen_pmtu_policies([0, 2]).unwrap();
            let repr = TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(1200),
                ..SEND_TEMPL
            };
            let ip = if ipv6 {
                IpRepr::Ipv6(crate::wire::Ipv6Repr {
                    src_addr: crate::wire::Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 2),
                    dst_addr: crate::wire::Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1),
                    next_header: IpProtocol::Tcp,
                    payload_len: repr.buffer_len(),
                    hop_limit: 64,
                })
            } else {
                IpRepr::Ipv4(crate::wire::Ipv4Repr {
                    src_addr: crate::wire::Ipv4Address::new(192, 0, 2, 2),
                    dst_addr: crate::wire::Ipv4Address::new(192, 0, 2, 1),
                    next_header: IpProtocol::Tcp,
                    payload_len: repr.buffer_len(),
                    hop_limit: 64,
                })
            };
            assert!(s.socket.accepts(&mut s.cx, &ip, &repr));
            s.socket.process(&mut s.cx, &ip, &repr);
            assert_eq!(s.state, State::SynReceived);
            assert_eq!(s.pmtu_discover(), if ipv6 { 2 } else { 0 });
            assert_eq!(
                s.local_endpoint().unwrap().addr.version(),
                if ipv6 {
                    IpVersion::Ipv6
                } else {
                    IpVersion::Ipv4
                }
            );
        }
    }

    #[test]
    #[cfg(feature = "packetmeta-id")]
    fn test_device_binding_handshake_snapshot_and_relisten() {
        let mut s = socket_listen();
        let a = NonZeroU32::new(7);
        let b = NonZeroU32::new(8);
        s.set_listen_bound_device(a);
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::SynReceived);
        s.set_listen_bound_device(b);
        assert_eq!(s.bound_device(), a);
        assert_eq!(s.egress_meta().id, 7);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.bound_device(), b);
        assert_eq!(s.lifecycle_device(), 8);
        assert!(!s.accepts_ingress(PacketMeta { id: 7 }));
        assert!(s.accepts_ingress(PacketMeta { id: 8 }));
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.bound_device(), b);
    }

    #[test]
    #[cfg(all(feature = "packetmeta-id", feature = "proto-ipv6"))]
    fn test_linklocal_passive_scope_and_relisten() {
        let mut s = socket_listen();
        s.listen_endpoint.addr = None;
        let syn = TcpRepr {
            control: TcpControl::Syn,
            seq_number: REMOTE_SEQ,
            ack_number: None,
            urgent_at: None,
            ..SEND_TEMPL
        };
        let ip = IpRepr::Ipv6(crate::wire::Ipv6Repr {
            src_addr: crate::wire::Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
            dst_addr: crate::wire::Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            next_header: IpProtocol::Tcp,
            payload_len: syn.buffer_len(),
            hop_limit: 64,
        });
        s.socket
            .process_with_meta(&mut s.cx, PacketMeta { id: 7 }, &ip, &syn);
        assert_eq!(s.state, State::SynReceived);
        assert_eq!(s.lifecycle_device(), 7);
        assert_eq!(s.egress_meta().id, 7);
        let rst = TcpRepr {
            control: TcpControl::Rst,
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(s.local_seq_no),
            urgent_at: None,
            ..SEND_TEMPL
        };
        s.socket
            .process_with_meta(&mut s.cx, PacketMeta { id: 7 }, &ip, &rst);
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.lifecycle_device(), 0);
        s.socket
            .process_with_meta(&mut s.cx, PacketMeta { id: 8 }, &ip, &syn);
        assert_eq!(s.state, State::SynReceived);
        assert_eq!(s.lifecycle_device(), 8);
        assert_eq!(s.egress_meta().id, 8);
    }

    #[test]
    #[cfg(all(feature = "packetmeta-id", feature = "proto-ipv6"))]
    fn test_passive_scope_preserves_explicit_policy_and_unscoped_peers() {
        for (prefix, explicit, expected) in [(0xfe80, 0, 7), (0xfe80, 8, 8), (0xfd00, 0, 0)] {
            let mut s = socket_listen();
            s.listen_endpoint.addr = None;
            s.set_listen_bound_device(NonZeroU32::new(explicit));
            let syn = TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            };
            let ip = IpRepr::Ipv6(crate::wire::Ipv6Repr {
                src_addr: crate::wire::Ipv6Address::new(prefix, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: crate::wire::Ipv6Address::new(prefix, 0, 0, 0, 0, 0, 0, 1),
                next_header: IpProtocol::Tcp,
                payload_len: syn.buffer_len(),
                hop_limit: 64,
            });
            let meta = PacketMeta {
                id: if explicit != 0 { explicit } else { 7 },
            };
            assert!(s.accepts_ingress(meta));
            if explicit != 0 {
                assert!(!s.accepts_ingress(PacketMeta { id: 7 }));
            }
            s.socket.process_with_meta(&mut s.cx, meta, &ip, &syn);
            assert_eq!(s.lifecycle_device(), expected);
        }
    }

    #[test]
    #[cfg(all(feature = "alloc", feature = "packetmeta-id", feature = "proto-ipv6"))]
    fn test_passive_scope_admission_precedes_connection_publication() {
        use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
        #[derive(Debug)]
        struct Observer {
            allow: AtomicBool,
            prepared: AtomicU32,
            published: AtomicU32,
        }
        impl LifecycleObserver for Observer {
            fn identity(&self) -> u64 {
                92
            }
            fn prepare_open(
                &self,
                _: IpEndpoint,
                _: IpEndpoint,
                device: u32,
                _: Option<u64>,
            ) -> bool {
                self.prepared.store(device, Ordering::Relaxed);
                self.allow.load(Ordering::Relaxed)
            }
            fn on_state_change(
                &self,
                _: State,
                _: Option<IpEndpoint>,
                _: Option<IpEndpoint>,
                device: u32,
            ) {
                self.published.store(device, Ordering::Relaxed);
            }
        }
        let observer = Shared::new(Observer {
            allow: AtomicBool::new(false),
            prepared: AtomicU32::new(0),
            published: AtomicU32::new(0),
        });
        let mut s = socket_listen();
        s.listen_endpoint.addr = None;
        s.set_lifecycle_observer(Some(observer.clone()));
        let syn = TcpRepr {
            control: TcpControl::Syn,
            seq_number: REMOTE_SEQ,
            ack_number: None,
            urgent_at: None,
            max_seg_size: Some(BASE_MSS),
            ..SEND_TEMPL
        };
        let ip = IpRepr::Ipv6(crate::wire::Ipv6Repr {
            src_addr: crate::wire::Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
            dst_addr: crate::wire::Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
            next_header: IpProtocol::Tcp,
            payload_len: syn.buffer_len(),
            hop_limit: 64,
        });
        s.socket
            .process_with_meta(&mut s.cx, PacketMeta { id: 7 }, &ip, &syn);
        assert_eq!(observer.prepared.load(Ordering::Relaxed), 7);
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.lifecycle_device(), 0);
        assert_eq!(observer.published.load(Ordering::Relaxed), 0);
        observer.allow.store(true, Ordering::Relaxed);
        observer.prepared.store(99, Ordering::Relaxed);
        let invalid = TcpRepr {
            max_seg_size: Some(0),
            ..syn
        };
        s.socket
            .process_with_meta(&mut s.cx, PacketMeta { id: 8 }, &ip, &invalid);
        assert_eq!(observer.prepared.load(Ordering::Relaxed), 99);
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.lifecycle_device(), 0);
        s.socket
            .process_with_meta(&mut s.cx, PacketMeta { id: 8 }, &ip, &syn);
        assert_eq!(observer.prepared.load(Ordering::Relaxed), 8);
        assert_eq!(observer.published.load(Ordering::Relaxed), 8);
        assert_eq!(s.lifecycle_device(), 8);
    }

    #[test]
    #[cfg(feature = "packetmeta-id")]
    fn test_device_binding_reset_and_abort() {
        let mut s = socket_established();
        s.set_bound_device(NonZeroU32::new(7));
        s.set_listen_bound_device(NonZeroU32::new(8));
        s.abort();
        assert_eq!(s.egress_meta().id, 7);
        s.reset();
        s.listen(80).unwrap();
        assert_eq!(s.bound_device(), NonZeroU32::new(8));
        s.set_bound_device(None);
        assert_eq!(s.egress_meta().id, 0);
        assert!(s.accepts_ingress(PacketMeta { id: 0 }));
        assert!(s.accepts_ingress(PacketMeta { id: 7 }));
    }

    #[test]
    #[cfg(feature = "packetmeta-id")]
    fn test_device_binding_simultaneous_open_keeps_active_device() {
        let mut s = socket_syn_sent();
        s.set_bound_device(NonZeroU32::new(7));
        s.set_listen_bound_device(NonZeroU32::new(8));
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::SynReceived);
        assert_eq!(s.bound_device(), NonZeroU32::new(7));
    }

    #[test]
    fn test_listen_syn_reject_ack() {
        let mut s = socket_listen();

        let tcp_repr = TcpRepr {
            control: TcpControl::Syn,
            seq_number: REMOTE_SEQ,
            ack_number: Some(LOCAL_SEQ),
            urgent_at: None,
            ..SEND_TEMPL
        };
        assert!(!s.socket.accepts(&mut s.cx, &SEND_IP_TEMPL, &tcp_repr));

        assert_eq!(s.state, State::Listen);
    }

    #[test]
    fn test_listen_rst() {
        let mut s = socket_listen();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.take_reset(), None);
    }

    #[test]
    fn test_listen_close() {
        let mut s = socket_listen();
        s.close();
        assert_eq!(s.state, State::Closed);
    }

    // =========================================================================================//
    // Tests for the SYN-RECEIVED state.
    // =========================================================================================//

    #[test]
    fn test_syn_received_ack() {
        let mut s = socket_syn_received();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Established);
        sanity!(s, socket_established());
    }

    #[test]
    fn test_syn_received_ack_too_low() {
        let mut s = socket_syn_received();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ), // wrong
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.state, State::SynReceived);
    }

    #[test]
    fn test_syn_received_ack_too_high() {
        let mut s = socket_syn_received();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 2), // wrong
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ + 2,
                ack_number: None,
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.state, State::SynReceived);
    }

    #[test]
    fn test_syn_received_fin() {
        let mut s = socket_syn_received();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6 + 1),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::CloseWait);

        let mut s2 = socket_close_wait();
        s2.remote_last_ack = Some(REMOTE_SEQ + 1 + 6 + 1);
        s2.remote_last_win = 58;
        sanity!(s, s2);
    }

    #[test]
    fn test_syn_received_rst() {
        let mut s = socket_syn_received();
        s.listen_endpoint = LISTEN_END;
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Listen);
        assert_eq!(s.listen_endpoint, LISTEN_END);
        assert_eq!(s.tuple, None);
        assert_eq!(s.take_reset(), None);
    }

    #[test]
    fn test_syn_received_no_window_scaling() {
        let mut s = socket_listen();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::SynReceived);
        assert_eq!(s.tuple, Some(TUPLE));
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                window_scale: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.remote_win_shift, 0);
        assert_eq!(s.remote_win_scale, None);
    }

    #[test]
    fn test_syn_received_window_scaling() {
        for scale in 0..14 {
            let mut s = socket_listen();
            send!(
                s,
                TcpRepr {
                    control: TcpControl::Syn,
                    seq_number: REMOTE_SEQ,
                    ack_number: None,
                    urgent_at: None,
                    window_scale: Some(scale),
                    ..SEND_TEMPL
                }
            );
            assert_eq!(s.state(), State::SynReceived);
            assert_eq!(s.tuple, Some(TUPLE));
            recv!(
                s,
                [TcpRepr {
                    control: TcpControl::Syn,
                    seq_number: LOCAL_SEQ,
                    ack_number: Some(REMOTE_SEQ + 1),
                    urgent_at: None,
                    max_seg_size: Some(BASE_MSS),
                    window_scale: Some(0),
                    ..RECV_TEMPL
                }]
            );
            send!(
                s,
                TcpRepr {
                    seq_number: REMOTE_SEQ + 1,
                    ack_number: Some(LOCAL_SEQ + 1),
                    urgent_at: None,
                    window_scale: None,
                    ..SEND_TEMPL
                }
            );
            assert_eq!(s.remote_win_scale, Some(scale));
        }
    }

    #[test]
    fn test_syn_received_close() {
        let mut s = socket_syn_received();
        s.close();
        assert_eq!(s.state, State::FinWait1);
    }

    // =========================================================================================//
    // Tests for the SYN-SENT state.
    // =========================================================================================//

    #[test]
    fn test_connect_validation() {
        let mut s = socket();
        assert_eq!(
            s.socket
                .connect(&mut s.cx, REMOTE_END, (IpvXAddress::UNSPECIFIED, 0)),
            Err(ConnectError::Unaddressable)
        );
        assert_eq!(
            s.socket
                .connect(&mut s.cx, REMOTE_END, (IpvXAddress::UNSPECIFIED, 1024)),
            Err(ConnectError::Unaddressable)
        );
        assert_eq!(
            s.socket
                .connect(&mut s.cx, (IpvXAddress::UNSPECIFIED, 0), LOCAL_END),
            Err(ConnectError::Unaddressable)
        );
        s.socket
            .connect(&mut s.cx, REMOTE_END, LOCAL_END)
            .expect("Connect failed with valid parameters");
        assert_eq!(s.tuple, Some(TUPLE));
    }

    #[test]
    fn test_connect() {
        let mut s = socket();
        s.local_seq_no = LOCAL_SEQ;
        s.socket
            .connect(&mut s.cx, REMOTE_END, LOCAL_END.port)
            .unwrap();
        assert_eq!(s.tuple, Some(TUPLE));
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.tuple, Some(TUPLE));
    }

    #[test]
    fn test_connect_unspecified_local() {
        let mut s = socket();
        assert_eq!(s.socket.connect(&mut s.cx, REMOTE_END, 80), Ok(()));
    }

    #[test]
    fn test_connect_specified_local() {
        let mut s = socket();
        assert_eq!(
            s.socket.connect(&mut s.cx, REMOTE_END, (REMOTE_ADDR, 80)),
            Ok(())
        );
    }

    #[test]
    fn test_connect_twice() {
        let mut s = socket();
        assert_eq!(s.socket.connect(&mut s.cx, REMOTE_END, 80), Ok(()));
        assert_eq!(
            s.socket.connect(&mut s.cx, REMOTE_END, 80),
            Err(ConnectError::InvalidState)
        );
    }

    #[test]
    fn test_syn_sent_sanity() {
        let mut s = socket();
        s.local_seq_no = LOCAL_SEQ;
        s.socket.connect(&mut s.cx, REMOTE_END, LOCAL_END).unwrap();
        sanity!(s, socket_syn_sent());
    }

    #[test]
    fn test_syn_sent_syn_ack() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        recv_nothing!(s, time 1000);
        assert_eq!(s.state, State::Established);
        sanity!(s, socket_established());
    }

    #[test]
    fn test_syn_sent_syn_received_ack() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );

        // A SYN packet changes the SYN-SENT state to SYN-RECEIVED.
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::SynReceived);

        // The socket will then send a SYN|ACK packet.
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                ..RECV_TEMPL
            }]
        );
        recv_nothing!(s);

        // The socket may retransmit the SYN|ACK packet.
        recv!(
            s,
            time 1001,
            Ok(TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                ..RECV_TEMPL
            })
        );

        // An ACK packet changes the SYN-RECEIVED state to ESTABLISHED.
        send!(
            s,
            TcpRepr {
                control: TcpControl::None,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Established);
        sanity!(s, socket_established());
    }

    #[test]
    fn test_syn_sent_syn_ack_not_incremented() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ), // WRONG
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.state, State::SynSent);
    }

    #[test]
    fn test_syn_sent_syn_received_rst() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );

        // A SYN packet changes the SYN-SENT state to SYN-RECEIVED.
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::SynReceived);

        // A RST packet changes the SYN-RECEIVED state to CLOSED.
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
        assert_eq!(s.take_reset(), Some(State::SynReceived));
    }

    #[test]
    fn test_syn_sent_rst() {
        let mut s = socket_syn_sent();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
        assert_eq!(s.take_reset(), Some(State::SynSent));
    }

    #[test]
    fn test_syn_sent_rst_no_ack() {
        let mut s = socket_syn_sent();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::SynSent);
        assert_eq!(s.take_reset(), None);
    }

    #[test]
    fn test_syn_sent_rst_bad_ack() {
        let mut s = socket_syn_sent();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ,
                ack_number: Some(TcpSeqNumber(1234)),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::SynSent);
        assert_eq!(s.take_reset(), None);
    }

    #[test]
    fn test_syn_sent_bad_ack() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::None, // Unexpected
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1), // Correct
                urgent_at: None,
                ..SEND_TEMPL
            }
        );

        // It should trigger no response and change no state
        recv!(s, []);
        assert_eq!(s.state, State::SynSent);
    }

    #[test]
    fn test_syn_sent_bad_ack_seq_1() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::None,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ), // WRONG
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ, // matching the ack_number of the unexpected ack
                ack_number: None,
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );

        // It should trigger a RST, and change no state
        assert_eq!(s.state, State::SynSent);
    }

    #[test]
    fn test_syn_sent_bad_ack_seq_2() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::None,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 123456), // WRONG
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ + 123456, // matching the ack_number of the unexpected ack
                ack_number: None,
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );

        // It should trigger a RST, and change no state
        assert_eq!(s.state, State::SynSent);
    }

    #[test]
    fn test_syn_sent_close() {
        let mut s = socket();
        s.close();
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_syn_sent_sack_option() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                sack_permitted: true,
                ..SEND_TEMPL
            }
        );
        assert!(s.remote_has_sack);

        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                sack_permitted: false,
                ..SEND_TEMPL
            }
        );
        assert!(!s.remote_has_sack);
    }

    #[test]
    fn test_syn_sent_win_scale_buffers() {
        for (buffer_size, shift_amt) in &[
            (64, 0),
            (128, 0),
            (1024, 0),
            (65535, 0),
            (65536, 1),
            (65537, 1),
            (131071, 1),
            (131072, 2),
            (524287, 3),
            (524288, 4),
            (655350, 4),
            (1048576, 5),
        ] {
            let mut s = socket_with_buffer_sizes(64, *buffer_size);
            s.local_seq_no = LOCAL_SEQ;
            assert_eq!(s.remote_win_shift, *shift_amt);
            s.socket.connect(&mut s.cx, REMOTE_END, LOCAL_END).unwrap();
            recv!(
                s,
                [TcpRepr {
                    control: TcpControl::Syn,
                    seq_number: LOCAL_SEQ,
                    ack_number: None,
                    urgent_at: None,
                    max_seg_size: Some(BASE_MSS),
                    window_scale: Some(*shift_amt),
                    window_len: u16::try_from(*buffer_size).unwrap_or(u16::MAX),
                    sack_permitted: true,
                    ..RECV_TEMPL
                }]
            );
        }
    }

    #[test]
    fn test_syn_sent_syn_ack_no_window_scaling() {
        let mut s = socket_syn_sent_with_buffer_sizes(1048576, 1048576);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                // scaling does NOT apply to the window value in SYN packets
                window_len: 65535,
                window_scale: Some(5),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.remote_win_shift, 5);
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: None,
                window_len: 42,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Established);
        assert_eq!(s.remote_win_shift, 0);
        assert_eq!(s.remote_win_scale, None);
        assert_eq!(s.remote_win_len, 42);
    }

    #[test]
    fn test_syn_sent_syn_ack_window_scaling() {
        let mut s = socket_syn_sent();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(7),
                window_len: 42,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Established);
        assert_eq!(s.remote_win_scale, Some(7));
        // scaling does NOT apply to the window value in SYN packets
        assert_eq!(s.remote_win_len, 42);
    }

    // =========================================================================================//
    // Tests for the ESTABLISHED state.
    // =========================================================================================//

    #[test]
    fn test_established_recv() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.rx_buffer.dequeue_many(6), &b"abcdef"[..]);
    }

    #[test]
    fn test_peek_slice() {
        const BUF_SIZE: usize = 10;

        let send_buf = b"0123456";

        let mut s = socket_established_with_buffer_sizes(BUF_SIZE, BUF_SIZE);

        // Populate the recv buffer
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &send_buf[..],
                ..SEND_TEMPL
            }
        );

        // Peek into the recv buffer
        let mut peeked_buf = [0u8; BUF_SIZE];
        let actually_peeked = s.peek_slice(&mut peeked_buf[..]).unwrap();
        let mut recv_buf = [0u8; BUF_SIZE];
        let actually_recvd = s.recv_slice(&mut recv_buf[..]).unwrap();
        assert_eq!(
            &mut peeked_buf[..actually_peeked],
            &mut recv_buf[..actually_recvd]
        );
    }

    #[test]
    fn test_peek_slice_buffer_wrap() {
        const BUF_SIZE: usize = 10;

        let send_buf = b"0123456789";

        let mut s = socket_established_with_buffer_sizes(BUF_SIZE, BUF_SIZE);

        let _ = s.rx_buffer.enqueue_slice(&send_buf[..8]);
        let _ = s.rx_buffer.dequeue_many(6);
        let _ = s.rx_buffer.enqueue_slice(&send_buf[..5]);

        let mut peeked_buf = [0u8; BUF_SIZE];
        let actually_peeked = s.peek_slice(&mut peeked_buf[..]).unwrap();
        let mut recv_buf = [0u8; BUF_SIZE];
        let actually_recvd = s.recv_slice(&mut recv_buf[..]).unwrap();
        assert_eq!(
            &mut peeked_buf[..actually_peeked],
            &mut recv_buf[..actually_recvd]
        );
    }

    #[test]
    fn test_peek_cursor_slice_offset_and_consumption() {
        let mut s = socket_established();
        assert_eq!(s.rx_buffer.enqueue_slice(b"abcdef"), 6);
        let head = s.recv_head_seq();
        let mut bytes = [b'!'; 8];
        assert_eq!(s.peek_slice_at(2, &mut bytes[..3]), Ok(3));
        assert_eq!(&bytes[..3], b"cde");
        assert_eq!(s.recv_queue(), 6);
        assert_eq!(s.recv_head_seq(), head);
        assert_eq!(s.recv_slice(&mut bytes[..2]), Ok(2));
        assert_eq!(s.recv_head_seq(), head + 2);
        assert_eq!(s.peek_slice_at(1, &mut bytes), Ok(3));
        assert_eq!(&bytes[..3], b"def");
        assert_eq!(s.recv_queue(), 4);
    }

    #[test]
    fn test_peek_cursor_slice_offset_wrap() {
        let mut s = socket_established_with_buffer_sizes(10, 10);
        assert_eq!(s.rx_buffer.enqueue_slice(b"01234567"), 8);
        let mut consumed = [0; 6];
        assert_eq!(s.recv_slice(&mut consumed), Ok(6));
        assert_eq!(s.rx_buffer.enqueue_slice(b"89012"), 5);
        let head = s.recv_head_seq();
        let mut bytes = [b'!'; 8];
        assert_eq!(s.peek_slice_at(1, &mut bytes), Ok(6));
        assert_eq!(&bytes[..6], b"789012");
        assert_eq!(&bytes[6..], b"!!");
        assert_eq!(s.recv_queue(), 7);
        assert_eq!(s.recv_head_seq(), head);
        assert_eq!(s.peek_slice(&mut bytes), Ok(7));
        assert_eq!(&bytes[..7], b"6789012");
    }

    #[test]
    fn test_peek_cursor_slice_offset_empty_and_out_of_bounds() {
        let mut s = socket_established_with_buffer_sizes(0, 0);
        let mut bytes = [b'!'; 4];
        for offset in [0, 1, usize::MAX] {
            assert_eq!(s.peek_slice_at(offset, &mut bytes), Ok(0));
            assert_eq!(&bytes, b"!!!!");
        }
        let mut s = socket_established();
        assert_eq!(s.rx_buffer.enqueue_slice(b"abc"), 3);
        let head = s.recv_head_seq();
        for offset in [3, 4, usize::MAX] {
            assert_eq!(s.peek_slice_at(offset, &mut bytes), Ok(0));
            assert_eq!(&bytes, b"!!!!");
        }
        assert_eq!(s.peek_slice_at(1, &mut []), Ok(0));
        assert_eq!(s.recv_head_seq(), head);
        assert_eq!(s.recv_queue(), 3);
    }

    #[test]
    fn test_peek_cursor_fin_keeps_queued_payload_head() {
        let mut s = socket_established();
        let head = s.recv_head_seq();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: b"abcdef",
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::CloseWait);
        assert_eq!(s.recv_head_seq(), head);
        let mut bytes = [0; 4];
        assert_eq!(s.peek_slice_at(2, &mut bytes), Ok(4));
        assert_eq!(&bytes, b"cdef");
        assert_eq!(s.recv_slice(&mut bytes), Ok(4));
        assert_eq!(s.recv_head_seq(), head + 4);
        assert_eq!(s.recv_slice(&mut bytes), Ok(2));
        assert_eq!(s.recv_head_seq(), head + 6);
        assert_eq!(s.recv_slice(&mut bytes), Err(RecvError::Finished));
        assert_eq!(s.recv_head_seq(), head + 6);
    }

    #[test]
    fn test_peek_cursor_head_sequence_wrap_with_fin() {
        let mut s = socket_established();
        s.remote_seq_no = TcpSeqNumber(-2);
        assert_eq!(s.rx_buffer.enqueue_slice(b"abcd"), 4);
        let head = s.recv_head_seq();
        let mut bytes = [0; 3];
        assert_eq!(s.recv_slice(&mut bytes), Ok(3));
        assert_eq!(s.recv_head_seq(), head + 3);
        s.remote_seq_no += 1;
        s.rx_fin_received = true;
        s.state = State::CloseWait;
        assert_eq!(s.recv_head_seq(), head + 3);
        assert_eq!(s.recv_slice(&mut bytes), Ok(1));
        assert_eq!(s.recv_head_seq(), head + 4);
    }

    #[test]
    fn test_peek_cursor_does_not_consume_reset_error() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        let pending = s.pending_error();
        assert!(pending.is_some());
        let head = s.recv_head_seq();
        let mut bytes = [b'!'; 4];
        assert_eq!(s.peek_slice(&mut bytes), Ok(0));
        assert_eq!(s.peek_slice_at(usize::MAX, &mut bytes), Ok(0));
        assert_eq!(&bytes, b"!!!!");
        assert_eq!(s.pending_error(), pending);
        assert_eq!(s.recv_head_seq(), head);
        assert_eq!(s.take_reset(), Some(State::Established));
        assert!(s.pending_error().is_none());
    }

    fn setup_rfc2018_cases() -> (TestSocket, Vec<u8>) {
        // This is a utility function used by the tests for RFC 2018 cases. It configures a socket
        // in a particular way suitable for those cases.
        //
        // RFC 2018: Assume the left window edge is 5000 and that the data transmitter sends [...]
        // segments, each containing 500 data bytes.
        let mut s = socket_established_with_buffer_sizes(4000, 4000);
        s.remote_has_sack = true;

        // create a segment that is 500 bytes long
        let mut segment: Vec<u8> = Vec::with_capacity(500);

        // move the last ack to 5000 by sending ten of them
        for _ in 0..50 {
            segment.extend_from_slice(b"abcdefghij")
        }
        for offset in (0..5000).step_by(500) {
            send!(
                s,
                TcpRepr {
                    seq_number: REMOTE_SEQ + 1 + offset,
                    ack_number: Some(LOCAL_SEQ + 1),
                    urgent_at: None,
                    payload: &segment,
                    ..SEND_TEMPL
                }
            );
            recv!(
                s,
                [TcpRepr {
                    seq_number: LOCAL_SEQ + 1,
                    ack_number: Some(REMOTE_SEQ + 1 + offset + 500),
                    urgent_at: None,
                    window_len: 3500,
                    ..RECV_TEMPL
                }]
            );
            s.recv(|data| {
                assert_eq!(data.len(), 500);
                assert_eq!(data, segment.as_slice());
                (500, ())
            })
            .unwrap();
        }
        assert_eq!(s.remote_last_win, 3500);
        (s, segment)
    }

    #[test]
    fn test_established_rfc2018_cases() {
        // This test case verifies the exact scenarios described on pages 8-9 of RFC 2018. Please
        // ensure its behavior does not deviate from those scenarios.

        let (mut s, segment) = setup_rfc2018_cases();
        // RFC 2018:
        //
        // Case 2: The first segment is dropped but the remaining 7 are received.
        //
        // Upon receiving each of the last seven packets, the data receiver will return a TCP ACK
        // segment that acknowledges sequence number 5000 and contains a SACK option specifying one
        // block of queued data:
        //
        //   Triggering   ACK      Left Edge  Right Edge
        //   Segment
        //
        //   5000         (lost)
        //   5500         5000     5500       6000
        //   6000         5000     5500       6500
        //   6500         5000     5500       7000
        //   7000         5000     5500       7500
        //   7500         5000     5500       8000
        //   8000         5000     5500       8500
        //   8500         5000     5500       9000
        //
        for offset in (500..3500).step_by(500) {
            send!(
                s,
                TcpRepr {
                    seq_number: REMOTE_SEQ + 1 + offset + 5000,
                    ack_number: Some(LOCAL_SEQ + 1),
                    urgent_at: None,
                    payload: &segment,
                    ..SEND_TEMPL
                },
                Some(TcpRepr {
                    seq_number: LOCAL_SEQ + 1,
                    ack_number: Some(REMOTE_SEQ + 1 + 5000),
                    urgent_at: None,
                    window_len: 4000,
                    sack_ranges: [
                        Some((
                            REMOTE_SEQ.0 as u32 + 1 + 5500,
                            REMOTE_SEQ.0 as u32 + 1 + 5500 + offset as u32
                        )),
                        None,
                        None
                    ],
                    ..RECV_TEMPL
                })
            );
        }
    }

    #[test]
    fn test_established_sliding_window_recv() {
        let mut s = socket_established();
        // Update our scaling parameters for a TCP with a scaled buffer.
        assert_eq!(s.rx_buffer.len(), 0);
        s.rx_buffer = SocketBuffer::new(vec![0; 262143]);
        s.assembler = Assembler::new();
        s.remote_win_scale = Some(0);
        s.remote_last_win = 65535 << 2;
        s.remote_win_shift = 2;

        // Create a TCP segment that will mostly fill an IP frame.
        let mut segment: Vec<u8> = Vec::with_capacity(1400);
        for _ in 0..100 {
            segment.extend_from_slice(b"abcdefghijklmn")
        }
        assert_eq!(segment.len(), 1400);

        // Send the frame
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &segment,
                ..SEND_TEMPL
            }
        );

        // Ensure that the received window size is shifted right by 2.
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1400),
                urgent_at: None,
                window_len: 65185,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_established_send() {
        let mut s = socket_established();
        // First roundtrip after establishing.
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.tx_buffer.len(), 6);
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.tx_buffer.len(), 0);
        // Second roundtrip.
        s.send_slice(b"foobar").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"foobar"[..],
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 6),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.tx_buffer.len(), 0);
    }

    #[test]
    fn test_established_send_no_ack_send() {
        let mut s = socket_established();
        s.set_nagle_enabled(false);
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        s.send_slice(b"foobar").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"foobar"[..],
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_established_send_buf_gt_win() {
        let mut data = [0; 32];
        for (i, elem) in data.iter_mut().enumerate() {
            *elem = i as u8
        }

        let mut s = socket_established();
        s.remote_win_len = 16;
        s.send_slice(&data[..]).unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &data[0..16],
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_established_send_window_shrink() {
        let mut s = socket_established();

        // 6 octets fit on the remote side's window, so we send them.
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.tx_buffer.len(), 6);

        println!(
            "local_seq_no={} remote_win_len={} remote_last_seq={}",
            s.local_seq_no, s.remote_win_len, s.remote_last_seq
        );

        // - Peer doesn't ack them yet
        // - Sends data so we need to reply with an ACK
        // - ...AND and sends a window announcement that SHRINKS the window, so data we've
        //   previously sent is now outside the window. Yes, this is allowed by TCP.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                window_len: 3,
                payload: &b"xyzxyz"[..],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.tx_buffer.len(), 6);

        println!(
            "local_seq_no={} remote_win_len={} remote_last_seq={}",
            s.local_seq_no, s.remote_win_len, s.remote_last_seq
        );

        // More data should not get sent since it doesn't fit in the window
        s.send_slice(b"foobar").unwrap();
        // Linux tcp_acceptable_seq() bounds a pure ACK to the peer's real
        // shrunken window edge (SND.UNA + SND.WND), not old SND.NXT.
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 3,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 64 - 6,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.remote_last_seq, LOCAL_SEQ + 1 + 6);
        assert_eq!(s.tx_buffer.len(), 12);
    }

    #[test]
    fn test_established_receive_partially_outside_window() {
        let mut s = socket_established();

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );

        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();

        // Peer decides to retransmit (perhaps because the ACK was lost)
        // and also pushed data.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );

        s.recv(|data| {
            assert_eq!(data, b"def");
            (3, ())
        })
        .unwrap();
    }

    #[test]
    fn test_established_receive_partially_outside_window_fin() {
        let mut s = socket_established();

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );

        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();

        // Peer decides to retransmit (perhaps because the ACK was lost)
        // and also pushed data, and sent a FIN.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                control: TcpControl::Fin,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );

        s.recv(|data| {
            assert_eq!(data, b"def");
            (3, ())
        })
        .unwrap();

        // We should accept the FIN, because even though the last packet was partially
        // outside the receive window, there is no hole after adding its data to the assembler.
        assert_eq!(s.state, State::CloseWait);
    }

    #[test]
    fn test_established_send_wrap() {
        let mut s = socket_established();
        let local_seq_start = TcpSeqNumber(i32::MAX - 1);
        s.local_seq_no = local_seq_start + 1;
        s.remote_last_seq = local_seq_start + 1;
        s.send_slice(b"abc").unwrap();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: local_seq_start + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_established_no_ack() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
    }

    #[test]
    fn test_established_bad_ack() {
        let mut s = socket_established();
        // Already acknowledged data.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(TcpSeqNumber(LOCAL_SEQ.0 - 1)),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.local_seq_no, LOCAL_SEQ + 1);
        // Data not yet transmitted.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 10),
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.local_seq_no, LOCAL_SEQ + 1);
    }

    #[test]
    fn test_established_bad_seq() {
        let mut s = socket_established();
        // Data outside of receive window.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 256,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.remote_seq_no, REMOTE_SEQ + 1);

        // Challenge ACKs are rate-limited, we don't get a second one immediately.
        send!(
            s,
            time 100,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 256,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );

        // If we wait a bit, we do get a new one.
        send!(
            s,
            time 2000,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 256,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.remote_seq_no, REMOTE_SEQ + 1);
    }

    #[test]
    fn test_established_fin() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::CloseWait);
        assert_eq!(s.take_reset(), None);
        sanity!(s, socket_close_wait());
    }

    #[test]
    fn test_established_fin_after_missing() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1 + 6,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"123456"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.state, State::Established);
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6 + 6),
                urgent_at: None,
                window_len: 52,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.state, State::Established);
    }

    #[test]
    fn test_established_send_fin() {
        let mut s = socket_established();
        s.send_slice(b"abcdef").unwrap();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::CloseWait);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_established_rst() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
        assert_eq!(s.reset_state(), Some(State::Established));
        assert_eq!(s.reset_state(), Some(State::Established));
        assert_eq!(s.take_reset(), Some(State::Established));
        assert_eq!(s.reset_state(), None);
        assert_eq!(s.take_reset(), None);
    }

    #[test]
    fn test_reset_event_reuse() {
        for reconnect in [false, true] {
            let mut s = socket_established();
            assert_eq!(s.reset_state(), None);
            send!(
                s,
                TcpRepr {
                    control: TcpControl::Rst,
                    seq_number: REMOTE_SEQ + 1,
                    ack_number: None,
                    urgent_at: None,
                    ..SEND_TEMPL
                }
            );
            assert_eq!(s.reset_state(), Some(State::Established));
            if reconnect {
                s.socket.connect(&mut s.cx, REMOTE_END, LOCAL_END).unwrap();
                assert_eq!(s.state, State::SynSent);
            } else {
                s.listen(LISTEN_END).unwrap();
                assert_eq!(s.state, State::Listen);
            }
            assert_eq!(s.take_reset(), None);
        }
    }

    #[test]
    fn test_close_wait_rst() {
        let mut s = socket_close_wait();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 2,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
        assert_eq!(s.take_reset(), Some(State::CloseWait));
    }

    #[test]
    fn test_established_rst_no_ack() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
        assert_eq!(s.take_reset(), Some(State::Established));
    }

    #[test]
    fn test_established_close() {
        let mut s = socket_established();
        s.close();
        assert_eq!(s.state, State::FinWait1);
        sanity!(s, socket_fin_wait_1());
    }

    #[test]
    fn test_fin_wait_receive_shutdown_data_resets() {
        let mut s = socket_fin_wait_1();
        s.shutdown_recv();

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"x"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                src_port: LOCAL_PORT,
                dst_port: REMOTE_PORT,
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ + 1,
                ack_number: None,
                urgent_at: None,
                window_len: 0,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &[],
            })
        );
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_established_abort() {
        let mut s = socket_established();
        s.abort();
        assert_eq!(s.state, State::Closed);
        assert_eq!(s.take_reset(), None);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Rst,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_established_rst_bad_seq() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ, // Wrong seq
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );

        assert_eq!(s.state, State::Established);
        assert_eq!(s.take_reset(), None);

        // Send something to advance seq by 1
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1, // correct seq
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"a"[..],
                ..SEND_TEMPL
            }
        );

        // Send wrong rst again, check that the challenge ack is correctly updated
        // The ack number must be updated even if we don't call dispatch on the socket
        // See https://github.com/smoltcp-rs/smoltcp/issues/338
        send!(
            s,
            time 2000,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ, // Wrong seq
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 2), // this has changed
                urgent_at: None,
                window_len: 63,
                ..RECV_TEMPL
            })
        );
    }

    // =========================================================================================//
    // Tests for the FIN-WAIT-1 state.
    // =========================================================================================//

    #[test]
    fn test_fin_wait_1_fin_ack() {
        let mut s = socket_fin_wait_1();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::FinWait2);
        sanity!(s, socket_fin_wait_2());
    }

    #[test]
    fn test_fin_wait_1_fin_fin() {
        let mut s = socket_fin_wait_1();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closing);
        sanity!(s, socket_closing());
    }

    #[test]
    fn test_fin_wait_1_fin_with_data_queued() {
        let mut s = socket_established();
        s.remote_win_len = 6;
        s.send_slice(b"abcdef123456").unwrap();
        s.close();
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            })
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::FinWait1);
    }

    #[test]
    fn test_fin_wait_1_recv() {
        let mut s = socket_fin_wait_1();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::FinWait1);
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
    }

    #[test]
    fn test_fin_wait_1_close() {
        let mut s = socket_fin_wait_1();
        s.close();
        assert_eq!(s.state, State::FinWait1);
    }

    // =========================================================================================//
    // Tests for the FIN-WAIT-2 state.
    // =========================================================================================//

    #[test]
    fn test_fin_wait_2_fin() {
        let mut s = socket_fin_wait_2();
        send!(s, time 1_000, TcpRepr {
            control: TcpControl::Fin,
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        assert_eq!(s.state, State::TimeWait);
        sanity!(s, socket_time_wait(false));
    }

    #[test]
    fn test_fin_wait_2_recv() {
        let mut s = socket_fin_wait_2();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::FinWait2);
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_fin_wait_2_close() {
        let mut s = socket_fin_wait_2();
        s.close();
        assert_eq!(s.state, State::FinWait2);
    }

    // =========================================================================================//
    // Tests for the CLOSING state.
    // =========================================================================================//

    #[test]
    fn test_closing_ack_fin() {
        let mut s = socket_closing();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(s, time 1_000, TcpRepr {
            seq_number: REMOTE_SEQ + 1 + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        assert_eq!(s.state, State::TimeWait);
        sanity!(s, socket_time_wait(true));
    }

    #[test]
    fn test_closing_close() {
        let mut s = socket_closing();
        s.close();
        assert_eq!(s.state, State::Closing);
    }

    // =========================================================================================//
    // Tests for the TIME-WAIT state.
    // =========================================================================================//

    #[test]
    fn test_time_wait_from_fin_wait_2_ack() {
        let mut s = socket_time_wait(false);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_time_wait_from_closing_no_ack() {
        let mut s = socket_time_wait(true);
        recv!(s, []);
    }

    #[test]
    fn test_time_wait_close() {
        let mut s = socket_time_wait(false);
        s.close();
        assert_eq!(s.state, State::TimeWait);
    }

    #[test]
    fn test_time_wait_retransmit() {
        let mut s = socket_time_wait(false);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(s, time 5_000, TcpRepr {
            control: TcpControl::Fin,
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 1),
            urgent_at: None,
            ..SEND_TEMPL
        }, Some(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        assert_eq!(
            s.timer,
            Timer::Close {
                expires_at: Instant::from_secs(5) + CLOSE_DELAY
            }
        );
    }

    #[test]
    fn test_time_wait_timeout() {
        let mut s = socket_time_wait(false);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::TimeWait);
        recv_nothing!(s, time 60_000);
        assert_eq!(s.state, State::Closed);
    }

    // =========================================================================================//
    // Tests for the CLOSE-WAIT state.
    // =========================================================================================//

    #[test]
    fn test_close_wait_ack() {
        let mut s = socket_close_wait();
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
    }

    #[test]
    fn test_close_wait_close() {
        let mut s = socket_close_wait();
        s.close();
        assert_eq!(s.state, State::LastAck);
        sanity!(s, socket_last_ack());
    }

    // =========================================================================================//
    // Tests for the LAST-ACK state.
    // =========================================================================================//
    #[test]
    fn test_last_ack_fin_ack() {
        let mut s = socket_last_ack();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::LastAck);
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_last_ack_ack_not_of_fin() {
        let mut s = socket_last_ack();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::LastAck);

        // ACK received that doesn't ack the FIN: socket should stay in LastAck.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::LastAck);

        // ACK received of fin: socket should change to Closed.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_last_ack_close() {
        let mut s = socket_last_ack();
        s.close();
        assert_eq!(s.state, State::LastAck);
    }

    // =========================================================================================//
    // Tests for transitioning through multiple states.
    // =========================================================================================//

    #[test]
    fn test_listen() {
        let mut s = socket();
        s.listen(LISTEN_END).unwrap();
        assert_eq!(s.state, State::Listen);
    }

    #[test]
    fn test_three_way_handshake() {
        let mut s = socket_listen();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::SynReceived);
        assert_eq!(s.tuple, Some(TUPLE));
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::Established);
        assert_eq!(s.local_seq_no, LOCAL_SEQ + 1);
        assert_eq!(s.remote_seq_no, REMOTE_SEQ + 1);
    }

    #[test]
    fn test_remote_close() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::CloseWait);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        s.close();
        assert_eq!(s.state, State::LastAck);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_local_close() {
        let mut s = socket_established();
        s.close();
        assert_eq!(s.state, State::FinWait1);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::FinWait2);
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_simultaneous_close() {
        let mut s = socket_established();
        s.close();
        assert_eq!(s.state, State::FinWait1);
        recv!(
            s,
            [TcpRepr {
                // due to reordering, this is logically located...
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closing);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        // ... at this point
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);
        recv!(s, []);
    }

    #[test]
    fn test_simultaneous_close_combined_fin_ack() {
        let mut s = socket_established();
        s.close();
        assert_eq!(s.state, State::FinWait1);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_simultaneous_close_raced() {
        let mut s = socket_established();
        s.close();
        assert_eq!(s.state, State::FinWait1);

        // Socket receives FIN before it has a chance to send its own FIN
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closing);

        // FIN + ack-of-FIN
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::Closing);

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);
        recv!(s, []);
    }

    #[test]
    fn test_simultaneous_close_raced_with_data() {
        let mut s = socket_established();
        s.send_slice(b"abcdef").unwrap();
        s.close();
        assert_eq!(s.state, State::FinWait1);

        // Socket receives FIN before it has a chance to send its own data+FIN
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closing);

        // data + FIN + ack-of-FIN
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::Closing);

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);
        recv!(s, []);
    }

    #[test]
    fn test_fin_with_data() {
        let mut s = socket_established();
        s.send_slice(b"abcdef").unwrap();
        s.close();
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        )
    }

    #[test]
    fn test_mutual_close_with_data_1() {
        let mut s = socket_established();
        s.send_slice(b"abcdef").unwrap();
        s.close();
        assert_eq!(s.state, State::FinWait1);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
    }

    #[test]
    fn test_mutual_close_with_data_2() {
        let mut s = socket_established();
        s.send_slice(b"abcdef").unwrap();
        s.close();
        assert_eq!(s.state, State::FinWait1);
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Fin,
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::FinWait2);
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6 + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 1),
                urgent_at: None,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.state, State::TimeWait);
    }

    // =========================================================================================//
    // Tests for retransmission on packet loss.
    // =========================================================================================//

    #[test]
    fn test_duplicate_seq_ack() {
        let mut s = socket_recved();
        // remote retransmission
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_data_retransmit() {
        let mut s = socket_established();
        s.send_slice(b"abcdef").unwrap();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
        recv_nothing!(s, time 1050);
        recv!(s, time 2000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_data_retransmit_bursts() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef012345").unwrap();

        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::None,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }), exact);
        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::Psh,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"012345"[..],
            ..RECV_TEMPL
        }), exact);
        recv_nothing!(s, time 0);

        recv_nothing!(s, time 50);

        recv!(s, time 1000, Ok(TcpRepr {
            control:    TcpControl::None,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }), exact);
        recv!(s, time 1500, Ok(TcpRepr {
            control:    TcpControl::Psh,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"012345"[..],
            ..RECV_TEMPL
        }), exact);
        recv_nothing!(s, time 1550);
    }

    #[test]
    fn test_data_retransmit_bursts_half_ack() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef012345").unwrap();

        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::None,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }), exact);
        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::Psh,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"012345"[..],
            ..RECV_TEMPL
        }), exact);
        // Acknowledge the first packet
        send!(s, time 5, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        // The second packet should be re-sent.
        recv!(s, time 1500, Ok(TcpRepr {
            control:    TcpControl::Psh,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"012345"[..],
            ..RECV_TEMPL
        }), exact);

        recv_nothing!(s, time 1550);
    }

    #[test]
    fn test_data_retransmit_bursts_half_ack_close() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef012345").unwrap();
        s.close();

        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::None,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }), exact);
        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"012345"[..],
            ..RECV_TEMPL
        }), exact);
        // Acknowledge the first packet
        send!(s, time 5, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        // The second packet should be re-sent.
        recv!(s, time 1500, Ok(TcpRepr {
            control:    TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"012345"[..],
            ..RECV_TEMPL
        }), exact);

        recv_nothing!(s, time 1550);
    }

    #[test]
    fn test_send_data_after_syn_ack_retransmit() {
        let mut s = socket_syn_received();
        recv!(s, time 50, Ok(TcpRepr {
            control:    TcpControl::Syn,
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            max_seg_size: Some(BASE_MSS),
            ..RECV_TEMPL
        }));
        recv!(s, time 750, Ok(TcpRepr { // retransmit
            control:    TcpControl::Syn,
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            max_seg_size: Some(BASE_MSS),
            ..RECV_TEMPL
        }));
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::Established);
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        )
    }

    #[test]
    fn test_established_retransmit_for_dup_ack() {
        let mut s = socket_established();
        // Duplicate ACKs do not replace the retransmission timer
        s.send_slice(b"abc").unwrap();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));
        // Retransmit timer is on because all data was sent
        assert_eq!(s.tx_buffer.len(), 3);
        // ACK nothing new
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        // Retransmit
        recv!(s, time 4000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_established_retransmit_reset_after_ack() {
        let mut s = socket_established();
        s.remote_win_len = 6;
        s.send_slice(b"abcdef").unwrap();
        s.send_slice(b"123456").unwrap();
        s.send_slice(b"ABCDEF").unwrap();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
        send!(s, time 1005, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        recv!(s, time 1010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"123456"[..],
            ..RECV_TEMPL
        }));
        send!(s, time 1015, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        recv!(s, time 1020, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"ABCDEF"[..],
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_established_queue_during_retransmission() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef123456ABCDEF").unwrap();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        })); // this one is dropped
        recv!(s, time 1005, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"123456"[..],
            ..RECV_TEMPL
        })); // this one is received
        recv!(s, time 1010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"ABCDEF"[..],
            ..RECV_TEMPL
        })); // also dropped
        recv!(s, time 2000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        })); // retransmission
        send!(s, time 2005, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6 + 6),
            urgent_at: None,
            ..SEND_TEMPL
        }); // acknowledgement of both segments
        recv!(s, time 2010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"ABCDEF"[..],
            ..RECV_TEMPL
        })); // retransmission of only unacknowledged data
    }

    #[test]
    fn test_close_wait_retransmit_reset_after_ack() {
        let mut s = socket_close_wait();
        s.remote_win_len = 6;
        s.send_slice(b"abcdef").unwrap();
        s.send_slice(b"123456").unwrap();
        s.send_slice(b"ABCDEF").unwrap();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
        send!(s, time 1005, TcpRepr {
            seq_number: REMOTE_SEQ + 1 + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        recv!(s, time 1010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            urgent_at: None,
            payload:    &b"123456"[..],
            ..RECV_TEMPL
        }));
        send!(s, time 1015, TcpRepr {
            seq_number: REMOTE_SEQ + 1 + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        recv!(s, time 1020, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6 + 6,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            urgent_at: None,
            payload:    &b"ABCDEF"[..],
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_fin_wait_1_retransmit_reset_after_ack() {
        let mut s = socket_established();
        s.remote_win_len = 6;
        s.send_slice(b"abcdef").unwrap();
        s.send_slice(b"123456").unwrap();
        s.send_slice(b"ABCDEF").unwrap();
        s.close();
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
        send!(s, time 1005, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        recv!(s, time 1010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"123456"[..],
            ..RECV_TEMPL
        }));
        send!(s, time 1015, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + 6 + 6),
            urgent_at: None,
            window_len: 6,
            ..SEND_TEMPL
        });
        recv!(s, time 1020, Ok(TcpRepr {
            control:    TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1 + 6 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"ABCDEF"[..],
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_fast_retransmit_after_triple_duplicate_ack() {
        let mut s = socket_established();
        s.remote_mss = 6;

        // Normal ACK of previously received segment
        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });

        // Send a long string of text divided into several packets
        // because of previously received "window_len"
        s.send_slice(b"xxxxxxyyyyyywwwwwwzzzzzz").unwrap();
        // This packet is lost
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"xxxxxx"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1005, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"yyyyyy"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + (6 * 2),
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"wwwwww"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1015, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + (6 * 3),
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"zzzzzz"[..],
            ..RECV_TEMPL
        }));

        // First duplicate ACK
        send!(s, time 1050, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        // Second duplicate ACK
        send!(s, time 1055, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        // Third duplicate ACK
        // Should trigger a fast retransmit of dropped packet
        send!(s, time 1060, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });

        // Fast retransmit packet
        recv!(s, time 1100, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"xxxxxx"[..],
            ..RECV_TEMPL
        }));

        recv!(s, time 1105, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"yyyyyy"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1110, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + (6 * 2),
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"wwwwww"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1115, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + (6 * 3),
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"zzzzzz"[..],
            ..RECV_TEMPL
        }));

        // After all was send out, enter *normal* retransmission,
        // don't stay in fast retransmission.
        assert!(match s.timer {
            Timer::Retransmit { expires_at, .. } => expires_at > Instant::from_millis(1115),
            _ => false,
        });

        // ACK all received segments
        send!(s, time 1120, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + (6 * 4)),
            urgent_at: None,
            ..SEND_TEMPL
        });
    }

    #[test]
    fn test_fast_retransmit_duplicate_detection_with_data() {
        let mut s = socket_established();

        s.send_slice(b"abc").unwrap(); // This is lost
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));

        // Normal ACK of previously received segment
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        // First duplicate
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        // Second duplicate
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );

        assert_eq!(s.local_rx_dup_acks, 2, "duplicate ACK counter is not set");

        // This packet has content, hence should not be detected
        // as a duplicate ACK and should reset the duplicate ACK count
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"xxxxxx"[..],
                ..SEND_TEMPL
            }
        );

        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 3,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            }]
        );

        assert_eq!(
            s.local_rx_dup_acks, 0,
            "duplicate ACK counter is not reset when receiving data"
        );
    }

    #[test]
    fn test_fast_retransmit_duplicate_detection_with_window_update() {
        let mut s = socket_established();

        s.send_slice(b"abc").unwrap(); // This is lost
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));

        // Normal ACK of previously received segment
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        // First duplicate
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        // Second duplicate
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );

        assert_eq!(s.local_rx_dup_acks, 2, "duplicate ACK counter is not set");

        // This packet has a window update, hence should not be detected
        // as a duplicate ACK and should reset the duplicate ACK count
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                window_len: 400,
                ..SEND_TEMPL
            }
        );

        assert_eq!(
            s.local_rx_dup_acks, 0,
            "duplicate ACK counter is not reset when receiving a window update"
        );
    }

    #[test]
    fn test_fast_retransmit_duplicate_detection() {
        let mut s = socket_established();
        s.remote_mss = 6;

        // Normal ACK of previously received segment
        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });

        // First duplicate, should not be counted as there is nothing to resend
        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });

        assert_eq!(
            s.local_rx_dup_acks, 0,
            "duplicate ACK counter is set but wound not transmit data"
        );

        // Send a long string of text divided into several packets
        // because of small remote_mss
        s.send_slice(b"xxxxxxyyyyyywwwwwwzzzzzz").unwrap();

        // This packet is reordered in network
        recv!(s, time 1000, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"xxxxxx"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1005, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"yyyyyy"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1010, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + (6 * 2),
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"wwwwww"[..],
            ..RECV_TEMPL
        }));
        recv!(s, time 1015, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1 + (6 * 3),
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"zzzzzz"[..],
            ..RECV_TEMPL
        }));

        // First duplicate ACK
        send!(s, time 1050, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        // Second duplicate ACK
        send!(s, time 1055, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        // Reordered packet arrives which should reset duplicate ACK count
        send!(s, time 1060, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + (6 * 3)),
            urgent_at: None,
            ..SEND_TEMPL
        });

        assert_eq!(
            s.local_rx_dup_acks, 0,
            "duplicate ACK counter is not reset when receiving ACK which updates send window"
        );

        // ACK all received segments
        send!(s, time 1120, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1 + (6 * 4)),
            urgent_at: None,
            ..SEND_TEMPL
        });
    }

    #[test]
    fn test_fast_retransmit_dup_acks_counter() {
        let mut s = socket_established();

        s.send_slice(b"abc").unwrap(); // This is lost
        recv!(s, time 0, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));

        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });

        // A lot of retransmits happen here
        s.local_rx_dup_acks = u8::MAX - 1;

        // Send 3 more ACKs, which could overflow local_rx_dup_acks,
        // but intended behaviour is that we saturate the bounds
        // of local_rx_dup_acks
        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        send!(s, time 0, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        assert_eq!(
            s.local_rx_dup_acks,
            u8::MAX,
            "duplicate ACK count should not overflow but saturate"
        );
    }

    #[test]
    fn test_fast_retransmit_zero_window() {
        let mut s = socket_established();

        send!(s, time 1000, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });

        s.send_slice(b"abc").unwrap();

        recv!(s, time 0, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abc"[..],
            ..RECV_TEMPL
        }));

        // 3 dup acks
        send!(s, time 1050, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        send!(s, time 1050, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        send!(s, time 1050, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            window_len: 0, // boom
            ..SEND_TEMPL
        });

        // even though we're in "fast retransmit", we shouldn't
        // force-send anything because the remote's window is full.
        recv_nothing!(s);
    }

    // =========================================================================================//
    // Tests for window management.
    // =========================================================================================//

    #[test]
    fn test_maximum_segment_size() {
        let mut s = socket_listen();
        s.tx_buffer = SocketBuffer::new(vec![0; 32767]);
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(1000),
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                window_len: 32767,
                ..SEND_TEMPL
            }
        );
        s.send_slice(&[0; 1200][..]).unwrap();
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &[0; 1000][..],
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_close_wait_no_window_update() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &[1, 2, 3, 4],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::CloseWait);

        // we ack the FIN, with the reduced window size.
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 6),
                urgent_at: None,
                window_len: 60,
                ..RECV_TEMPL
            })
        );

        let rx_buf = &mut [0; 32];
        assert_eq!(s.recv_slice(rx_buf), Ok(4));

        // check that we do NOT send a window update even if it has changed.
        recv_nothing!(s);
    }

    #[test]
    fn test_time_wait_no_window_update() {
        let mut s = socket_fin_wait_2();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 2),
                urgent_at: None,
                payload: &[1, 2, 3, 4],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);

        // we ack the FIN, with the reduced window size.
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 2,
                ack_number: Some(REMOTE_SEQ + 6),
                urgent_at: None,
                window_len: 60,
                ..RECV_TEMPL
            })
        );

        let rx_buf = &mut [0; 32];
        assert_eq!(s.recv_slice(rx_buf), Ok(4));

        // check that we do NOT send a window update even if it has changed.
        recv_nothing!(s);
    }

    // =========================================================================================//
    // Tests for flow control.
    // =========================================================================================//

    #[test]
    fn test_psh_transmit() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef").unwrap();
        s.send_slice(b"123456").unwrap();
        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::None,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }), exact);
        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::Psh,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"123456"[..],
            ..RECV_TEMPL
        }), exact);
    }

    #[test]
    fn test_psh_receive() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Psh,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_zero_window_ack() {
        let mut s = socket_established();
        s.rx_buffer = SocketBuffer::new(vec![0; 6]);
        s.assembler = Assembler::new();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 6,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"123456"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_zero_window_fin() {
        let mut s = socket_established();
        s.rx_buffer = SocketBuffer::new(vec![0; 6]);
        s.assembler = Assembler::new();
        s.ack_delay = None;

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            }]
        );

        // Even though the sequence space for the FIN itself is outside the window,
        // it is not data, so FIN must be accepted when window full.
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 6,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &[],
                control: TcpControl::Fin,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::CloseWait);

        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 7),
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_zero_window_ack_on_window_growth() {
        let mut s = socket_established();
        s.rx_buffer = SocketBuffer::new(vec![0; 6]);
        s.assembler = Assembler::new();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            }]
        );
        recv_nothing!(s, time 0);
        s.recv(|buffer| {
            assert_eq!(&buffer[..3], b"abc");
            (3, ())
        })
        .unwrap();
        recv!(s, time 0, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 3,
            ..RECV_TEMPL
        }));
        recv_nothing!(s, time 0);
        s.recv(|buffer| {
            assert_eq!(buffer, b"def");
            (buffer.len(), ())
        })
        .unwrap();
        recv!(s, time 0, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 6),
            urgent_at: None,
            window_len: 6,
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_window_update_with_delay_ack() {
        let mut s = socket_established_with_buffer_sizes(6, 6);
        s.ack_delay = Some(Duration::from_millis(10));

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            }
        );

        recv_nothing!(s, time 5);

        s.recv(|buffer| {
            assert_eq!(&buffer[..2], b"ab");
            (2, ())
        })
        .unwrap();
        recv!(
            s,
            time 5,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 2,
                ..RECV_TEMPL
            })
        );

        s.recv(|buffer| {
            assert_eq!(&buffer[..1], b"c");
            (1, ())
        })
        .unwrap();
        recv_nothing!(s, time 5);

        s.recv(|buffer| {
            assert_eq!(&buffer[..1], b"d");
            (1, ())
        })
        .unwrap();
        recv!(
            s,
            time 5,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 4,
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_fill_peer_window() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef123456!@#$%^").unwrap();
        recv!(
            s,
            [
                TcpRepr {
                    seq_number: LOCAL_SEQ + 1,
                    ack_number: Some(REMOTE_SEQ + 1),
                    urgent_at: None,
                    payload: &b"abcdef"[..],
                    ..RECV_TEMPL
                },
                TcpRepr {
                    seq_number: LOCAL_SEQ + 1 + 6,
                    ack_number: Some(REMOTE_SEQ + 1),
                    urgent_at: None,
                    payload: &b"123456"[..],
                    ..RECV_TEMPL
                },
                TcpRepr {
                    seq_number: LOCAL_SEQ + 1 + 6 + 6,
                    ack_number: Some(REMOTE_SEQ + 1),
                    urgent_at: None,
                    payload: &b"!@#$%^"[..],
                    ..RECV_TEMPL
                }
            ]
        );
    }

    #[test]
    fn test_announce_window_after_read() {
        let mut s = socket_established();
        s.rx_buffer = SocketBuffer::new(vec![0; 6]);
        s.assembler = Assembler::new();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                window_len: 3,
                ..RECV_TEMPL
            }]
        );
        // Test that `dispatch` updates `remote_last_win`
        assert_eq!(s.remote_last_win, s.rx_buffer.window());
        s.recv(|buffer| (buffer.len(), ())).unwrap();
        assert!(s.window_to_update());
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                window_len: 6,
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.remote_last_win, s.rx_buffer.window());
        // Provoke immediate ACK to test that `process` updates `remote_last_win`
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 6,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"def"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                window_len: 6,
                ..RECV_TEMPL
            })
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 3,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 9),
                urgent_at: None,
                window_len: 0,
                ..RECV_TEMPL
            })
        );
        assert_eq!(s.remote_last_win, s.rx_buffer.window());
        s.recv(|buffer| (buffer.len(), ())).unwrap();
        assert!(s.window_to_update());
    }

    #[test]
    fn test_last_scaled_window_returns_zero_on_invalid_last_window() {
        let mut s = socket_established();
        s.remote_last_ack = Some(s.remote_seq_no);
        s.remote_last_win = 0;
        assert_eq!(s.rx_buffer.enqueue_slice(&[0u8; 4]), 4);

        assert_eq!(s.last_scaled_window(), Some(0));
    }

    // =========================================================================================//
    // Tests for timeouts.
    // =========================================================================================//

    #[test]
    fn test_listen_timeout() {
        let mut s = socket_listen();
        s.set_timeout(Some(Duration::from_millis(100)));
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Ingress);
    }

    #[test]
    fn test_connect_timeout() {
        let mut s = socket();
        s.local_seq_no = LOCAL_SEQ;
        s.socket
            .connect(&mut s.cx, REMOTE_END, LOCAL_END.port)
            .unwrap();
        s.set_timeout(Some(Duration::from_millis(100)));
        recv!(s, time 150, Ok(TcpRepr {
            control:    TcpControl::Syn,
            seq_number: LOCAL_SEQ,
            ack_number: None,
            urgent_at: None,
            max_seg_size: Some(BASE_MSS),
            window_scale: Some(0),
            sack_permitted: true,
            ..RECV_TEMPL
        }));
        assert_eq!(s.state, State::SynSent);
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(250))
        );
        recv!(s, time 250, Ok(TcpRepr {
            control:    TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(TcpSeqNumber(0)),
            urgent_at: None,
            window_scale: None,
            ..RECV_TEMPL
        }));
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_established_timeout() {
        let mut s = socket_established();
        s.set_timeout(Some(Duration::from_millis(1000)));
        recv_nothing!(s, time 250);
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(1250))
        );
        s.send_slice(b"abcdef").unwrap();
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Now);
        recv!(s, time 255, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(955))
        );
        recv!(s, time 955, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }));
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(1255))
        );
        recv!(s, time 1255, Ok(TcpRepr {
            control:    TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_established_keep_alive_timeout() {
        let mut s = socket_established();
        s.set_keep_alive(Some(Duration::from_millis(50)));
        s.set_timeout(Some(Duration::from_millis(100)));
        recv!(s, time 100, Ok(TcpRepr {
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &[0],
            ..RECV_TEMPL
        }));
        recv_nothing!(s, time 100);
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(150))
        );
        send!(s, time 105, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(155))
        );
        recv!(s, time 155, Ok(TcpRepr {
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &[0],
            ..RECV_TEMPL
        }));
        recv_nothing!(s, time 155);
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(205))
        );
        recv_nothing!(s, time 200);
        recv!(s, time 205, Ok(TcpRepr {
            control:    TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        recv_nothing!(s, time 205);
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_fin_wait_1_timeout() {
        let mut s = socket_fin_wait_1();
        s.set_timeout(Some(Duration::from_millis(1000)));
        recv!(s, time 100, Ok(TcpRepr {
            control:    TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        recv!(s, time 1100, Ok(TcpRepr {
            control:    TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1 + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_last_ack_timeout() {
        let mut s = socket_last_ack();
        s.set_timeout(Some(Duration::from_millis(1000)));
        recv!(s, time 100, Ok(TcpRepr {
            control:    TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        recv!(s, time 1100, Ok(TcpRepr {
            control:    TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1 + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        assert_eq!(s.state, State::Closed);
    }

    #[test]
    fn test_closed_timeout() {
        let mut s = socket_established();
        s.set_timeout(Some(Duration::from_millis(200)));
        s.remote_last_ts = Some(Instant::from_millis(100));
        s.abort();
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Now);
        recv!(s, time 100, Ok(TcpRepr {
            control:    TcpControl::Rst,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            ..RECV_TEMPL
        }));
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Ingress);
    }

    // =========================================================================================//
    // Tests for keep-alive.
    // =========================================================================================//

    #[test]
    fn test_responds_to_keep_alive() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_sends_keep_alive() {
        let mut s = socket_established();
        s.set_keep_alive(Some(Duration::from_millis(100)));

        // drain the forced keep-alive packet
        assert_eq!(s.socket.poll_at(&mut s.cx), PollAt::Now);
        recv!(s, time 0, Ok(TcpRepr {
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &[0],
            ..RECV_TEMPL
        }));

        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(100))
        );
        recv_nothing!(s, time 95);
        recv!(s, time 100, Ok(TcpRepr {
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &[0],
            ..RECV_TEMPL
        }));

        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(200))
        );
        recv_nothing!(s, time 195);
        recv!(s, time 200, Ok(TcpRepr {
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &[0],
            ..RECV_TEMPL
        }));

        send!(s, time 250, TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            ..SEND_TEMPL
        });
        assert_eq!(
            s.socket.poll_at(&mut s.cx),
            PollAt::Time(Instant::from_millis(350))
        );
        recv_nothing!(s, time 345);
        recv!(s, time 350, Ok(TcpRepr {
            seq_number: LOCAL_SEQ,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"\x00"[..],
            ..RECV_TEMPL
        }));
    }

    // =========================================================================================//
    // Tests for time-to-live configuration.
    // =========================================================================================//

    #[test]
    fn test_set_hop_limit() {
        let mut s = socket_syn_received();

        s.set_hop_limit(Some(0x2a));
        assert_eq!(
            s.socket.dispatch(&mut s.cx, |_, (ip_repr, _)| {
                assert_eq!(ip_repr.hop_limit(), 0x2a);
                Ok::<_, ()>(())
            }),
            Ok(())
        );

        // assert that user-configurable settings are kept,
        // see https://github.com/smoltcp-rs/smoltcp/issues/601.
        s.reset();
        assert_eq!(s.hop_limit(), Some(0x2a));
    }

    #[test]
    #[should_panic(expected = "the time-to-live value of a packet must not be zero")]
    fn test_set_hop_limit_zero() {
        let mut s = socket_syn_received();
        s.set_hop_limit(Some(0));
    }

    // =========================================================================================//
    // Tests for reassembly.
    // =========================================================================================//

    #[test]
    fn test_out_of_order() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 3,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"def"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                ..RECV_TEMPL
            })
        );
        s.recv(|buffer| {
            assert_eq!(buffer, b"");
            (buffer.len(), ())
        })
        .unwrap();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 6),
                urgent_at: None,
                window_len: 58,
                ..RECV_TEMPL
            })
        );
        s.recv(|buffer| {
            assert_eq!(buffer, b"abcdef");
            (buffer.len(), ())
        })
        .unwrap();
    }

    #[test]
    fn test_buffer_wraparound_rx() {
        let mut s = socket_established();
        s.rx_buffer = SocketBuffer::new(vec![0; 6]);
        s.assembler = Assembler::new();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        s.recv(|buffer| {
            assert_eq!(buffer, b"abc");
            (buffer.len(), ())
        })
        .unwrap();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 3,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"defghi"[..],
                ..SEND_TEMPL
            }
        );
        let mut data = [0; 6];
        assert_eq!(s.recv_slice(&mut data[..]), Ok(6));
        assert_eq!(data, &b"defghi"[..]);
    }

    #[test]
    fn test_buffer_wraparound_tx() {
        let mut s = socket_established();
        s.set_nagle_enabled(false);

        s.tx_buffer = SocketBuffer::new(vec![b'.'; 9]);
        assert_eq!(s.send_slice(b"xxxyyy"), Ok(6));
        assert_eq!(s.tx_buffer.dequeue_many(3), &b"xxx"[..]);
        assert_eq!(s.tx_buffer.len(), 3);

        // "abcdef" not contiguous in tx buffer
        assert_eq!(s.send_slice(b"abcdef"), Ok(6));
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"yyyabc"[..],
                ..RECV_TEMPL
            })
        );
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"def"[..],
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_dispatch_clamps_remote_last_seq_before_sending() {
        let mut s = socket_established();
        s.set_nagle_enabled(false);

        let local_seq = s.local_seq_no;
        s.remote_last_seq = local_seq - 5;

        assert_eq!(s.send_slice(b"abc"), Ok(3));
        recv!(s, time 0, Ok(TcpRepr {
            control: TcpControl::Psh,
            seq_number: local_seq,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload: &b"abc"[..],
            ..RECV_TEMPL
        }), exact);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_set_buffer_size_updates_capacity() {
        let mut s = socket_established();
        s.set_send_buffer_size(128).unwrap();
        s.set_recv_buffer_size(256).unwrap();
        assert_eq!(s.tx_buffer.capacity(), 128);
        assert_eq!(s.rx_buffer.capacity(), 256);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_set_send_buffer_size_does_not_shrink_below_length() {
        let mut s = socket_established_with_buffer_sizes(16, 64);
        assert_eq!(s.send_slice(b"abcdef"), Ok(6));
        s.set_send_buffer_size(4).unwrap();
        assert_eq!(s.tx_buffer.capacity(), 16);
    }

    #[test]
    fn test_logical_send_shrink_retains_data_and_restricts_both_enqueue_apis() {
        let mut s = socket_established_with_buffer_sizes(16, 64);
        assert_eq!(s.send_slice(b"abcdef"), Ok(6));
        assert!(s.set_send_buffer_limit(4));
        assert_eq!(s.send_queue(), 6);
        assert_eq!(s.send_capacity(), 16);
        assert!(!s.can_send());
        assert_eq!(s.send_slice(b"extra"), Ok(0));
        assert_eq!(
            s.send(|bytes| {
                assert!(bytes.is_empty());
                (0, ())
            }),
            Ok(())
        );
        let _ = s.tx_buffer.dequeue_many(4);
        assert!(s.can_send());
        assert_eq!(s.send_slice(b"12345"), Ok(2));
        let mut bytes = [0; 4];
        assert_eq!(s.tx_buffer.read_allocated(0, &mut bytes), 4);
        assert_eq!(&bytes, b"ef12");
        let _ = s.tx_buffer.dequeue_many(2);
        assert_eq!(
            s.send(|bytes| {
                assert_eq!(bytes.len(), 2);
                bytes.copy_from_slice(b"XY");
                (2, 17)
            }),
            Ok(17)
        );
        assert!(!s.set_send_buffer_limit(17));
        assert!(!s.can_send());
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_logical_receive_syn_window_unaligned_ack_and_wrap() {
        for base in [TcpSeqNumber(1000), TcpSeqNumber(i32::MAX - 5)] {
            let mut s = Socket::new(
                SocketBuffer::new(vec![0; 131072]),
                SocketBuffer::new(vec![0; 16]),
            );
            assert_eq!(s.remote_win_shift, 2);
            assert!(s.set_receive_buffer_limit(131072));
            assert_eq!(s.desired_receive_window(), 131069);
            s.remote_seq_no = base;
            s.state = State::SynReceived;
            let syn = TcpRepr {
                control: TcpControl::Syn,
                ack_number: Some(base),
                urgent_at: None,
                window_len: 65535,
                ..RECV_TEMPL
            };
            s.remember_advertised_window(&syn);
            assert_eq!(s.remote_last_win, 65535, "SYN window must not be scaled");
            let original_edge = base + s.remote_last_win;
            s.state = State::Established;
            let _ = s.rx_buffer.enqueue_slice(b"q");
            assert!(s.set_receive_buffer_limit(1));
            let window = usize::from(s.scaled_window()) << s.remote_win_shift;
            let ack = base + s.rx_buffer.len();
            assert!(ack + window >= original_edge);
            assert!(window <= s.rx_buffer.window());
            let update = TcpRepr {
                control: TcpControl::None,
                ack_number: Some(ack),
                urgent_at: None,
                window_len: s.scaled_window(),
                ..RECV_TEMPL
            };
            s.remember_advertised_window(&update);
            assert_eq!(s.remote_last_win, window);
            let promised_capacity = s.receive_storage_min_capacity();
            let candidate = SocketBuffer::new(vec![0; promised_capacity - 1]);
            assert_eq!(
                s.replace_prepared_receive_buffer(candidate)
                    .unwrap_err()
                    .capacity(),
                promised_capacity - 1
            );
            let _ = s.rx_buffer.enqueue_slice(&vec![7; window]);
            assert_eq!(
                s.scaled_window(),
                0,
                "promise converges without discarding queued bytes"
            );
            assert_eq!(s.rx_buffer.len(), window + 1);
        }
        let mut existing = socket_established();
        assert!(
            !existing.set_receive_buffer_limit(1),
            "first activation is pre-open only"
        );
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_prepared_buffers_preserve_wrapped_data_and_reassembly() {
        let mut s = socket_established_with_buffer_sizes(16, 16);
        let _ = s.rx_buffer.enqueue_slice(b"abcdefghijklmn");
        let _ = s.rx_buffer.dequeue_many(12);
        let _ = s.rx_buffer.enqueue_slice(b"op");
        // The assembler is relative to the unallocated window, not read_at.
        let _ = s.rx_buffer.write_unallocated(3, b"XYZ");
        s.assembler.add(3, 3).unwrap();
        let _ = s.tx_buffer.enqueue_slice(b"abcdefghijklmn");
        let _ = s.tx_buffer.dequeue_many(12);
        let _ = s.tx_buffer.enqueue_slice(b"opqrst");
        let before_seq = (s.local_seq_no, s.remote_seq_no, s.remote_last_seq);
        let old = s
            .replace_prepared_buffers(
                SocketBuffer::new(vec![0; 32]),
                SocketBuffer::new(vec![0; 32]),
            )
            .unwrap();
        assert_eq!(old.0.capacity(), 16);
        assert_eq!(old.1.capacity(), 16);
        let mut bytes = [0u8; 8];
        assert_eq!(s.rx_buffer.read_allocated(0, &mut bytes), 4);
        assert_eq!(&bytes[..4], b"mnop");
        assert_eq!(s.tx_buffer.read_allocated(0, &mut bytes), 8);
        assert_eq!(&bytes, b"mnopqrst");
        assert_eq!(s.assembler.iter_data(0).collect::<Vec<_>>(), vec![(3, 6)]);
        let _ = s.rx_buffer.write_unallocated(0, b"123");
        assert_eq!(s.assembler.add_then_remove_front(0, 3), Ok(6));
        s.rx_buffer.enqueue_unallocated(6);
        let mut received = [0u8; 10];
        assert_eq!(s.rx_buffer.read_allocated(0, &mut received), 10);
        assert_eq!(&received, b"mnop123XYZ");
        assert_eq!(
            before_seq,
            (s.local_seq_no, s.remote_seq_no, s.remote_last_seq)
        );
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_single_axis_prepared_buffers_preserve_other_storage_and_holes() {
        let mut s = socket_established_with_buffer_sizes(16, 16);
        let _ = s.rx_buffer.enqueue_slice(b"abcdefghijklmn");
        let _ = s.rx_buffer.dequeue_many(12);
        let _ = s.rx_buffer.enqueue_slice(b"op");
        let _ = s.rx_buffer.write_unallocated(3, b"XYZ");
        s.assembler.add(3, 3).unwrap();
        let _ = s.tx_buffer.enqueue_slice(b"abcdefghijklmn");
        let _ = s.tx_buffer.dequeue_many(12);
        let _ = s.tx_buffer.enqueue_slice(b"opqrst");
        let tx_ptr = s.tx_buffer.get_unallocated(0, 0).as_ptr();
        let old_rx = s
            .replace_prepared_receive_buffer(SocketBuffer::new(vec![0; 32]))
            .unwrap();
        assert_eq!(old_rx.capacity(), 16);
        assert_eq!(s.tx_buffer.get_unallocated(0, 0).as_ptr(), tx_ptr);
        assert_eq!(s.assembler.iter_data(0).collect::<Vec<_>>(), vec![(3, 6)]);
        let rx_ptr = s.rx_buffer.get_unallocated(0, 0).as_ptr();
        let old_tx = s
            .replace_prepared_send_buffer(SocketBuffer::new(vec![0; 32]))
            .unwrap();
        assert_eq!(old_tx.capacity(), 16);
        assert_eq!(s.rx_buffer.get_unallocated(0, 0).as_ptr(), rx_ptr);
        let mut bytes = [0; 8];
        assert_eq!(s.tx_buffer.read_allocated(0, &mut bytes), 8);
        assert_eq!(&bytes, b"mnopqrst");
        let candidate = s
            .replace_prepared_receive_buffer(SocketBuffer::new(vec![0; 9]))
            .unwrap_err();
        assert_eq!(candidate.capacity(), 9);
        assert!(candidate.is_empty());
        assert_eq!(s.rx_buffer.capacity(), 32);
        assert_eq!(s.assembler.iter_data(0).collect::<Vec<_>>(), vec![(3, 6)]);
    }

    #[test]
    #[cfg(feature = "alloc")]
    fn test_prepared_buffers_reject_both_atomically() {
        let mut s = socket_established_with_buffer_sizes(16, 16);
        let _ = s.rx_buffer.enqueue_slice(b"read");
        let _ = s.rx_buffer.write_unallocated(4, b"HOLE");
        s.assembler.add(4, 4).unwrap();
        let _ = s.tx_buffer.enqueue_slice(b"transmit");
        for (rx_size, tx_size, nonempty) in [(11, 16, false), (16, 7, false), (16, 16, true)] {
            let mut rx = SocketBuffer::new(vec![0; rx_size]);
            let tx = SocketBuffer::new(vec![0; tx_size]);
            if nonempty {
                let _ = rx.enqueue_slice(b"candidate");
            }
            let candidates = s.replace_prepared_buffers(rx, tx).unwrap_err();
            assert_eq!(candidates.0.capacity(), rx_size);
            assert_eq!(candidates.1.capacity(), tx_size);
            assert_eq!(candidates.0.len(), if nonempty { 9 } else { 0 });
            assert_eq!((s.rx_buffer.capacity(), s.tx_buffer.capacity()), (16, 16));
            let mut bytes = [0; 8];
            assert_eq!(s.tx_buffer.read_allocated(0, &mut bytes), 8);
            assert_eq!(&bytes, b"transmit");
            assert_eq!(s.rx_buffer.len(), 4);
            assert_eq!(s.assembler.iter_data(0).collect::<Vec<_>>(), vec![(4, 8)]);
        }
    }

    // =========================================================================================//
    // Tests for graceful vs ungraceful rx close
    // =========================================================================================//

    #[test]
    fn test_rx_close_fin() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        assert_eq!(s.recv(|_| (0, ())), Err(RecvError::Finished));
    }

    #[test]
    fn test_rx_close_fin_in_fin_wait_1() {
        let mut s = socket_fin_wait_1();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::Closing);
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        assert_eq!(s.recv(|_| (0, ())), Err(RecvError::Finished));
    }

    #[test]
    fn test_rx_close_fin_in_fin_wait_2() {
        let mut s = socket_fin_wait_2();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state, State::TimeWait);
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        assert_eq!(s.recv(|_| (0, ())), Err(RecvError::Finished));
    }

    #[test]
    fn test_rx_close_fin_with_hole() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Fin,
                seq_number: REMOTE_SEQ + 1 + 6,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"ghi"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                window_len: 61,
                ..RECV_TEMPL
            })
        );
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        s.recv(|data| {
            assert_eq!(data, b"");
            (0, ())
        })
        .unwrap();
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1 + 9,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        // Error must be `Illegal` even if we've received a FIN,
        // because we are missing data.
        assert_eq!(s.recv(|_| (0, ())), Err(RecvError::InvalidState));
    }

    #[test]
    fn test_rx_close_rst() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1 + 3,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        assert_eq!(s.recv(|_| (0, ())), Err(RecvError::InvalidState));
    }

    #[test]
    fn test_rx_close_rst_with_hole() {
        let mut s = socket_established();
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + 6,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"ghi"[..],
                ..SEND_TEMPL
            },
            Some(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                window_len: 61,
                ..RECV_TEMPL
            })
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Rst,
                seq_number: REMOTE_SEQ + 1 + 9,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();
        assert_eq!(s.recv(|_| (0, ())), Err(RecvError::InvalidState));
    }

    // =========================================================================================//
    // Tests for delayed ACK
    // =========================================================================================//

    #[test]
    fn test_delayed_ack() {
        let mut s = socket_established();
        s.set_ack_delay(Some(ACK_DELAY_DEFAULT));
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );

        // No ACK is immediately sent.
        recv_nothing!(s);

        // After 10ms, it is sent.
        recv!(s, time 11, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 3),
            urgent_at: None,
            window_len: 61,
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_delayed_ack_win() {
        let mut s = socket_established();
        s.set_ack_delay(Some(ACK_DELAY_DEFAULT));
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );

        // Reading the data off the buffer should cause a window update.
        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();

        // However, no ACK or window update is immediately sent.
        recv_nothing!(s);

        // After 10ms, it is sent.
        recv!(s, time 11, Ok(TcpRepr {
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1 + 3),
            urgent_at: None,
            ..RECV_TEMPL
        }));
    }

    #[test]
    fn test_delayed_ack_reply() {
        let mut s = socket_established();
        s.set_ack_delay(Some(ACK_DELAY_DEFAULT));
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"abc"[..],
                ..SEND_TEMPL
            }
        );

        s.recv(|data| {
            assert_eq!(data, b"abc");
            (3, ())
        })
        .unwrap();

        s.send_slice(&b"xyz"[..]).unwrap();

        // Writing data to the socket causes ACK to not be delayed,
        // because it is immediately sent with the data.
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + 3),
                urgent_at: None,
                payload: &b"xyz"[..],
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_delayed_ack_every_rmss() {
        let mut s = socket_established_with_buffer_sizes(DEFAULT_MSS * 2, DEFAULT_MSS * 2);
        s.set_ack_delay(Some(ACK_DELAY_DEFAULT));
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &[0; DEFAULT_MSS - 1],
                ..SEND_TEMPL
            }
        );

        // No ACK is immediately sent.
        recv_nothing!(s);

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + (DEFAULT_MSS - 1),
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"a"[..],
                ..SEND_TEMPL
            }
        );

        // No ACK is immediately sent.
        recv_nothing!(s);

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + DEFAULT_MSS,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"a"[..],
                ..SEND_TEMPL
            }
        );

        // RMSS+1 bytes of data has been received, so ACK is sent without delay.
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + (DEFAULT_MSS + 1)),
                urgent_at: None,
                window_len: (DEFAULT_MSS - 1) as u16,
                ..RECV_TEMPL
            })
        );
    }

    #[test]
    fn test_delayed_ack_every_rmss_or_more() {
        let mut s = socket_established_with_buffer_sizes(DEFAULT_MSS * 2, DEFAULT_MSS * 2);
        s.set_ack_delay(Some(ACK_DELAY_DEFAULT));
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &[0; DEFAULT_MSS],
                ..SEND_TEMPL
            }
        );

        // No ACK is immediately sent.
        recv_nothing!(s);

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + DEFAULT_MSS,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"a"[..],
                ..SEND_TEMPL
            }
        );

        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1 + (DEFAULT_MSS + 1),
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                payload: &b"b"[..],
                ..SEND_TEMPL
            }
        );

        // RMSS+2 bytes of data has been received, so ACK is sent without delay.
        recv!(
            s,
            Ok(TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1 + (DEFAULT_MSS + 2)),
                urgent_at: None,
                window_len: (DEFAULT_MSS - 2) as u16,
                ..RECV_TEMPL
            })
        );
    }

    // =========================================================================================//
    // Tests for Nagle's Algorithm
    // =========================================================================================//

    #[test]
    fn test_nagle() {
        let mut s = socket_established();
        s.remote_mss = 6;

        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                ..RECV_TEMPL
            }]
        );

        // If there's data in flight, full segments get sent.
        s.send_slice(b"foobar").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"foobar"[..],
                ..RECV_TEMPL
            }]
        );

        s.send_slice(b"aaabbbccc").unwrap();
        // If there's data in flight, not-full segments don't get sent.
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"aaabbb"[..],
                ..RECV_TEMPL
            }]
        );

        // Data gets ACKd, so there's no longer data in flight
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 6 + 6),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );

        // Now non-full segment gets sent.
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6 + 6 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"ccc"[..],
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_final_packet_in_stream_doesnt_wait_for_nagle() {
        let mut s = socket_established();
        s.remote_mss = 6;
        s.send_slice(b"abcdef0").unwrap();
        s.socket.close();

        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::None,
            seq_number: LOCAL_SEQ + 1,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"abcdef"[..],
            ..RECV_TEMPL
        }), exact);
        recv!(s, time 0, Ok(TcpRepr {
            control:    TcpControl::Fin,
            seq_number: LOCAL_SEQ + 1 + 6,
            ack_number: Some(REMOTE_SEQ + 1),
            urgent_at: None,
            payload:    &b"0"[..],
            ..RECV_TEMPL
        }), exact);
    }

    // =========================================================================================//
    // Tests for packet filtering.
    // =========================================================================================//

    #[test]
    fn test_doesnt_accept_wrong_port() {
        let mut s = socket_established();
        s.rx_buffer = SocketBuffer::new(vec![0; 6]);
        s.assembler = Assembler::new();

        let tcp_repr = TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            dst_port: LOCAL_PORT + 1,
            ..SEND_TEMPL
        };
        assert!(!s.socket.accepts(&mut s.cx, &SEND_IP_TEMPL, &tcp_repr));

        let tcp_repr = TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            src_port: REMOTE_PORT + 1,
            ..SEND_TEMPL
        };
        assert!(!s.socket.accepts(&mut s.cx, &SEND_IP_TEMPL, &tcp_repr));
    }

    #[test]
    fn test_doesnt_accept_wrong_ip() {
        let mut s = socket_established();

        let tcp_repr = TcpRepr {
            seq_number: REMOTE_SEQ + 1,
            ack_number: Some(LOCAL_SEQ + 1),
            urgent_at: None,
            payload: &b"abcdef"[..],
            ..SEND_TEMPL
        };

        let ip_repr = IpReprIpvX(IpvXRepr {
            src_addr: REMOTE_ADDR,
            dst_addr: LOCAL_ADDR,
            next_header: IpProtocol::Tcp,
            payload_len: tcp_repr.buffer_len(),
            hop_limit: 64,
        });
        assert!(s.socket.accepts(&mut s.cx, &ip_repr, &tcp_repr));

        let ip_repr_wrong_src = IpReprIpvX(IpvXRepr {
            src_addr: OTHER_ADDR,
            dst_addr: LOCAL_ADDR,
            next_header: IpProtocol::Tcp,
            payload_len: tcp_repr.buffer_len(),
            hop_limit: 64,
        });
        assert!(!s.socket.accepts(&mut s.cx, &ip_repr_wrong_src, &tcp_repr));

        let ip_repr_wrong_dst = IpReprIpvX(IpvXRepr {
            src_addr: REMOTE_ADDR,
            dst_addr: OTHER_ADDR,
            next_header: IpProtocol::Tcp,
            payload_len: tcp_repr.buffer_len(),
            hop_limit: 64,
        });
        assert!(!s.socket.accepts(&mut s.cx, &ip_repr_wrong_dst, &tcp_repr));
    }

    // =========================================================================================//
    // Timer tests
    // =========================================================================================//

    #[test]
    fn test_timer_retransmit() {
        const RTO: Duration = Duration::from_millis(100);
        let mut r = Timer::new();
        assert_eq!(r.should_retransmit(Instant::from_secs(1)), None);
        r.set_for_retransmit(Instant::from_millis(1000), RTO);
        assert_eq!(r.should_retransmit(Instant::from_millis(1000)), None);
        assert_eq!(r.should_retransmit(Instant::from_millis(1050)), None);
        assert_eq!(
            r.should_retransmit(Instant::from_millis(1101)),
            Some(Duration::from_millis(101))
        );
        r.set_for_retransmit(Instant::from_millis(1101), RTO);
        assert_eq!(r.should_retransmit(Instant::from_millis(1101)), None);
        assert_eq!(r.should_retransmit(Instant::from_millis(1150)), None);
        assert_eq!(r.should_retransmit(Instant::from_millis(1200)), None);
        assert_eq!(
            r.should_retransmit(Instant::from_millis(1301)),
            Some(Duration::from_millis(300))
        );
        r.set_for_idle(Instant::from_millis(1301), None);
        assert_eq!(r.should_retransmit(Instant::from_millis(1350)), None);
    }

    #[test]
    fn test_rtt_estimator() {
        let mut r = RttEstimator::default();

        let rtos = &[
            751, 766, 755, 731, 697, 656, 613, 567, 523, 484, 445, 411, 378, 350, 322, 299, 280,
            261, 243, 229, 215, 206, 197, 188,
        ];

        for &rto in rtos {
            r.sample(100);
            assert_eq!(r.retransmission_timeout(), Duration::from_millis(rto));
        }
    }

    #[test]
    fn test_set_get_congestion_control() {
        let mut s = socket_established();

        #[cfg(feature = "socket-tcp-reno")]
        {
            s.set_congestion_control(CongestionControl::Reno);
            assert_eq!(s.congestion_control(), CongestionControl::Reno);
        }

        #[cfg(feature = "socket-tcp-cubic")]
        {
            s.set_congestion_control(CongestionControl::Cubic);
            assert_eq!(s.congestion_control(), CongestionControl::Cubic);
        }

        s.set_congestion_control(CongestionControl::None);
        assert_eq!(s.congestion_control(), CongestionControl::None);
    }

    // =========================================================================================//
    // Timestamp tests
    // =========================================================================================//

    #[test]
    fn test_tsval_established_connection() {
        let mut s = socket_established();
        s.set_tsval_generator(Some(|| 1));

        assert!(s.timestamp_enabled());

        // First roundtrip after establishing.
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                timestamp: Some(TcpTimestampRepr::new(1, 0)),
                ..RECV_TEMPL
            }]
        );
        assert_eq!(s.tx_buffer.len(), 6);
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6),
                urgent_at: None,
                timestamp: Some(TcpTimestampRepr::new(500, 1)),
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.tx_buffer.len(), 0);
        // Second roundtrip.
        s.send_slice(b"foobar").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1 + 6,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"foobar"[..],
                timestamp: Some(TcpTimestampRepr::new(1, 500)),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1 + 6 + 6),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.tx_buffer.len(), 0);
    }

    #[test]
    fn test_tsval_disabled_in_remote_client() {
        let mut s = socket_listen();
        s.set_tsval_generator(Some(|| 1));
        assert!(s.timestamp_enabled());
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::SynReceived);
        assert_eq!(s.tuple, Some(TUPLE));
        assert!(!s.timestamp_enabled());
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::Established);
        assert_eq!(s.local_seq_no, LOCAL_SEQ + 1);
        assert_eq!(s.remote_seq_no, REMOTE_SEQ + 1);
    }

    #[test]
    fn test_tsval_disabled_in_local_server() {
        let mut s = socket_listen();
        // s.set_timestamp(false); // commented to alert if the default state changes
        assert!(!s.timestamp_enabled());
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: None,
                urgent_at: None,
                timestamp: Some(TcpTimestampRepr::new(500, 0)),
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::SynReceived);
        assert_eq!(s.tuple, Some(TUPLE));
        assert!(!s.timestamp_enabled());
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                seq_number: REMOTE_SEQ + 1,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                ..SEND_TEMPL
            }
        );
        assert_eq!(s.state(), State::Established);
        assert_eq!(s.local_seq_no, LOCAL_SEQ + 1);
        assert_eq!(s.remote_seq_no, REMOTE_SEQ + 1);
    }

    #[test]
    fn test_tsval_disabled_in_remote_server() {
        let mut s = socket();
        s.set_tsval_generator(Some(|| 1));
        assert!(s.timestamp_enabled());
        s.local_seq_no = LOCAL_SEQ;
        s.socket
            .connect(&mut s.cx, REMOTE_END, LOCAL_END.port)
            .unwrap();
        assert_eq!(s.tuple, Some(TUPLE));
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                timestamp: Some(TcpTimestampRepr::new(1, 0)),
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                timestamp: None,
                ..SEND_TEMPL
            }
        );
        assert!(!s.timestamp_enabled());
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                timestamp: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn test_tsval_disabled_in_local_client() {
        let mut s = socket();
        // s.set_timestamp(false); // commented to alert if the default state changes
        assert!(!s.timestamp_enabled());
        s.local_seq_no = LOCAL_SEQ;
        s.socket
            .connect(&mut s.cx, REMOTE_END, LOCAL_END.port)
            .unwrap();
        assert_eq!(s.tuple, Some(TUPLE));
        recv!(
            s,
            [TcpRepr {
                control: TcpControl::Syn,
                seq_number: LOCAL_SEQ,
                ack_number: None,
                urgent_at: None,
                max_seg_size: Some(BASE_MSS),
                window_scale: Some(0),
                sack_permitted: true,
                ..RECV_TEMPL
            }]
        );
        send!(
            s,
            TcpRepr {
                control: TcpControl::Syn,
                seq_number: REMOTE_SEQ,
                ack_number: Some(LOCAL_SEQ + 1),
                urgent_at: None,
                max_seg_size: Some(BASE_MSS - 80),
                window_scale: Some(0),
                timestamp: Some(TcpTimestampRepr::new(500, 0)),
                ..SEND_TEMPL
            }
        );
        assert!(!s.timestamp_enabled());
        s.send_slice(b"abcdef").unwrap();
        recv!(
            s,
            [TcpRepr {
                seq_number: LOCAL_SEQ + 1,
                ack_number: Some(REMOTE_SEQ + 1),
                urgent_at: None,
                payload: &b"abcdef"[..],
                timestamp: None,
                ..RECV_TEMPL
            }]
        );
    }

    #[test]
    fn control_error_active_handshake_closes_without_sending_rst() {
        let mut s = socket_syn_sent();
        s.report_network_error(NetworkError::ConnectionRefused, false, false);
        assert_eq!(s.state(), State::Closed);
        assert_eq!(s.reset_state(), None);
        assert_eq!(
            s.pending_error(),
            Some(PendingError {
                reason: ErrorReason::Network(NetworkError::ConnectionRefused),
                state: State::SynSent,
            })
        );
        recv_nothing!(s);
        assert!(s.take_error().is_some());
        assert_eq!(s.take_error(), None);
    }

    #[test]
    fn control_error_established_soft_and_reported_do_not_close() {
        let mut s = socket_established();
        s.report_network_error(NetworkError::HostUnreachable, false, false);
        assert_eq!(s.state(), State::Established);
        assert_eq!(s.take_error(), None);
        assert_eq!(s.take_soft_error(), Some(NetworkError::HostUnreachable));
        assert_eq!(s.take_soft_error(), None);
        s.report_network_error(NetworkError::ConnectionRefused, true, false);
        assert_eq!(s.state(), State::Established);
        assert!(s.can_send());
        assert_eq!(
            s.take_error().unwrap().reason,
            ErrorReason::Network(NetworkError::ConnectionRefused)
        );
        assert_eq!(s.send_slice(b"still alive"), Ok(11));
    }

    #[test]
    fn control_error_passive_request_policy_recycles_without_error() {
        let mut s = socket_syn_received();
        s.listen_endpoint = LOCAL_END.into();
        s.admitted_seq_end = Some(s.local_seq_no + 1);
        assert!(s.accepts_error_quote(TUPLE.local, TUPLE.remote, s.local_seq_no, 0));
        assert!(!s.accepts_error_quote(TUPLE.local, TUPLE.remote, s.local_seq_no + 1, 0));
        s.report_network_error(NetworkError::ConnectionRefused, false, false);
        assert_eq!(s.state(), State::SynReceived);
        s.report_network_error(NetworkError::NetworkUnreachable, true, true);
        assert_eq!(s.state(), State::Listen);
        assert_eq!(s.take_error(), None);
        assert_eq!(s.take_soft_error(), None);
        assert_eq!(s.tuple, None);
    }

    #[test]
    fn control_error_reuse_clears_soft_and_reported_errors() {
        let mut s = socket_established();
        s.report_network_error(NetworkError::HostUnreachable, false, false);
        s.report_network_error(NetworkError::ConnectionRefused, true, false);
        s.reset();
        assert_eq!(s.take_error(), None);
        assert_eq!(s.take_soft_error(), None);
    }

    #[test]
    fn control_error_real_timeout_promotes_soft_reason() {
        let mut s = socket_established();
        s.send_slice(b"pending").unwrap();
        s.set_timeout(Some(Duration::from_millis(100)));
        pmtu_dispatch(&mut s, 1500, 0, TcpPacketAdmission::Admitted);
        s.report_network_error(NetworkError::HostUnreachable, false, false);
        assert_eq!(s.state(), State::Established);
        s.cx.set_now(Instant::from_millis(100));
        pmtu_dispatch(&mut s, 1500, 100, TcpPacketAdmission::Admitted);
        assert_eq!(s.state(), State::Closed);
        assert_eq!(
            s.take_error().unwrap().reason,
            ErrorReason::Network(NetworkError::HostUnreachable)
        );
        assert_eq!(s.take_soft_error(), None);
    }
    fn urgent_input(
        s: &mut TestSocket,
        sequence: TcpSeqNumber,
        payload: &[u8],
        pointer: Option<u16>,
    ) {
        let repr = TcpRepr {
            seq_number: sequence,
            ack_number: Some(s.local_seq_no),
            payload,
            urgent_at: pointer,
            ..SEND_TEMPL
        };
        send(s, Instant::from_millis(0), &repr);
    }

    #[test]
    fn urgent_byte_shares_sequence_and_normal_boundary() {
        let mut s = socket_established();
        let head = s.recv_head_seq();
        urgent_input(&mut s, head, b"abcXYdef", Some(5));
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'Y')));
        assert!(s.take_urgent_notification());
        assert!(!s.take_urgent_notification());
        assert_eq!(s.ordinary_queue(), 4);
        assert_eq!(
            s.receive_span(0, 64),
            ReceiveSpan {
                offset: 0,
                len: 4,
                boundary: true
            }
        );
        let mut prefix = [0; 4];
        assert_eq!(s.recv_slice(&mut prefix), Ok(4));
        assert_eq!(&prefix, b"abcX");
        assert!(s.at_urgent_mark());
        assert_eq!(
            s.receive_span(0, 64),
            ReceiveSpan {
                offset: 1,
                len: 3,
                boundary: false
            }
        );
        assert_eq!(s.ordinary_queue(), 0);
        s.consume_urgent();
        assert_eq!(s.urgent_state(), Some(UrgentState::Read));
        assert!(s.at_urgent_mark());
        let _ = s.recv(|bytes| (1.min(bytes.len()), ()));
        assert_eq!(s.urgent_state(), None);
        let mut suffix = [0; 3];
        assert_eq!(s.recv_slice(&mut suffix), Ok(3));
        assert_eq!(&suffix, b"def");
    }

    #[test]
    fn urgent_pending_then_unflagged_data_and_duplicate() {
        let mut s = socket_established();
        let head = s.recv_head_seq();
        urgent_input(&mut s, head, &[], Some(4));
        assert_eq!(s.urgent_state(), Some(UrgentState::Pending));
        assert!(s.take_urgent_notification());
        urgent_input(&mut s, head, &[], Some(4));
        assert!(!s.take_urgent_notification());
        urgent_input(&mut s, head, b"abcd", None);
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'd')));
        assert_eq!(s.recv_head_seq(), head);
    }

    #[test]
    fn urgent_inline_preserves_byte_and_still_stops_prefix() {
        let mut s = socket_established();
        s.set_urgent_inline(true);
        let head = s.recv_head_seq();
        urgent_input(&mut s, head, b"abc!def", Some(4));
        assert_eq!(s.ordinary_queue(), 7);
        assert_eq!(
            s.receive_span(0, 64),
            ReceiveSpan {
                offset: 0,
                len: 3,
                boundary: true
            }
        );
        s.recv(|_| (3, ())).unwrap();
        assert!(s.at_urgent_mark());
        assert_eq!(
            s.receive_span(0, 64),
            ReceiveSpan {
                offset: 0,
                len: 4,
                boundary: false
            }
        );
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'!')));
        s.recv(|_| (1, ())).unwrap();
        assert_eq!(s.urgent_state(), None);
    }

    #[test]
    fn urgent_only_byte_not_ordinary_readiness_and_peek_is_pure() {
        let mut s = socket_established();
        let head = s.recv_head_seq();
        urgent_input(&mut s, head, b"!", Some(1));
        assert!(s.can_recv());
        assert!(!s.ordinary_can_recv());
        assert_eq!(s.receive_span(0, 5).len, 0);
        assert_eq!(s.recv_head_seq(), head);
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'!')));
        s.set_urgent_inline(true);
        assert!(s.ordinary_can_recv());
    }

    #[test]
    fn urgent_new_mark_retires_old_hole_at_head() {
        let mut s = socket_established();
        let head = s.recv_head_seq();
        urgent_input(&mut s, head, b"A", Some(1));
        urgent_input(&mut s, head + 1, b"B", Some(1));
        assert_eq!(s.recv_head_seq(), head + 1);
        assert_eq!(s.recv_queue(), 1);
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'B')));
        assert!(s.at_urgent_mark());
    }

    #[test]
    fn urgent_out_of_order_wrap_and_old_pointer_ignored() {
        let mut s = socket_established();
        s.remote_seq_no = TcpSeqNumber(i32::MAX - 2);
        let head = s.recv_head_seq();
        urgent_input(&mut s, head + 3, b"!def", Some(1));
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'!')));
        assert_eq!(s.recv_queue(), 0);
        urgent_input(&mut s, head, b"abc", None);
        assert_eq!(s.recv_queue(), 7);
        assert_eq!(s.ordinary_queue(), 3);
        s.take_urgent_notification();
        urgent_input(&mut s, head + 7, &[], Some(0));
        assert_eq!(s.urgent_state(), Some(UrgentState::Pending));
        assert!(s.take_urgent_notification());
        assert_eq!(s.receive_span(0, 64).len, 7);
    }

    #[test]
    fn urgent_out_of_window_does_not_publish_mark() {
        let mut s = socket_established();
        let head = s.recv_head_seq();
        urgent_input(&mut s, head + 128, b"!", Some(1));
        assert_eq!(s.urgent_state(), None);
        assert!(!s.take_urgent_notification());
    }

    #[test]
    fn urgent_send_partial_marks_only_accepted_prefix_and_reset_clears() {
        let mut s = socket_established_with_buffer_sizes(4, 64);
        let start = s.local_seq_no;
        assert_eq!(s.send_slice_with_urgent(b"abcdef", Some(6)), Ok(4));
        assert_eq!(s.tx_urgent_end, Some(start + 4));
        s.reset();
        assert_eq!(s.tx_urgent_end, None);
        assert_eq!(s.urgent_state(), None);
        assert!(!s.take_urgent_notification());
    }

    #[test]
    fn urgent_send_wire_retransmit_and_ack_retirement() {
        let mut s = socket_established();
        let start = s.local_seq_no;
        assert_eq!(s.send_slice_with_urgent(b"ab!", Some(3)), Ok(3));
        let mut observed = None;
        s.socket
            .dispatch(&mut s.cx, |_, (_, repr)| {
                observed = Some((repr.seq_number, repr.urgent_at, repr.payload.to_vec()));
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(observed, Some((start, Some(3), b"ab!".to_vec())));
        s.remote_last_seq = start;
        s.timer.set_for_fast_retransmit();
        let mut pointer = None;
        s.socket
            .dispatch(&mut s.cx, |_, (_, repr)| {
                pointer = repr.urgent_at;
                Ok::<_, ()>(())
            })
            .unwrap();
        assert_eq!(pointer, Some(3));
        let head = s.recv_head_seq();
        let repr = TcpRepr {
            seq_number: head,
            ack_number: Some(start + 3),
            ..SEND_TEMPL
        };
        send(&mut s, Instant::from_millis(1), &repr);
        assert_eq!(s.tx_urgent_end, None);
    }
    #[test]
    fn urgent_ring_wrap_span_and_payload_stay_in_one_sequence_space() {
        let mut s = socket_established_with_buffer_sizes(8, 8);
        let start = s.recv_head_seq();
        urgent_input(&mut s, start, b"abcdef", None);
        let mut old = [0; 5];
        assert_eq!(s.recv_slice(&mut old), Ok(5));
        urgent_input(&mut s, start + 6, b"ghi!jk", Some(4));
        assert_eq!(s.ordinary_queue(), 4);
        assert_eq!(s.receive_span(0, 64).len, 4);
        assert!(s.receive_span(0, 64).boundary);
        let mut prefix = [0; 4];
        assert_eq!(s.peek_slice_at(0, &mut prefix), Ok(4));
        assert_eq!(&prefix, b"fghi");
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'!')));
        assert_eq!(s.recv_head_seq(), start + 5);
    }

    #[test]
    fn urgent_pending_survives_fin_and_reset_for_owner_error_order() {
        for control in [TcpControl::Fin, TcpControl::Rst] {
            let mut s = socket_established();
            let start = s.recv_head_seq();
            urgent_input(&mut s, start, &[], Some(4));
            assert_eq!(s.urgent_state(), Some(UrgentState::Pending));
            let packet = TcpRepr {
                seq_number: start,
                ack_number: Some(s.local_seq_no),
                control,
                ..SEND_TEMPL
            };
            send(&mut s, Instant::from_millis(0), &packet);
            assert_eq!(s.urgent_state(), Some(UrgentState::Pending));
            assert_eq!(s.received_fin(), control == TcpControl::Fin);
            assert_eq!(
                s.state(),
                if control == TcpControl::Fin {
                    State::CloseWait
                } else {
                    State::Closed
                }
            );
        }
    }

    #[test]
    fn urgent_read_byte_can_be_overwritten_by_a_new_pointer_and_old_replay_is_ignored() {
        let mut s = socket_established();
        let start = s.recv_head_seq();
        urgent_input(&mut s, start, b"abc!", Some(4));
        s.consume_urgent();
        s.take_urgent_notification();
        urgent_input(&mut s, start + 4, b"x", Some(0));
        // ptr=0 denotes this segment's first byte under default Linux semantics.
        assert_eq!(s.urgent_state(), Some(UrgentState::Valid(b'x')));
        assert!(s.take_urgent_notification());
        urgent_input(&mut s, start + 5, &[], Some(0));
        s.take_urgent_notification();
        let packet = TcpRepr {
            seq_number: start + 5,
            ack_number: Some(s.local_seq_no),
            urgent_at: Some(0),
            ..SEND_TEMPL
        };
        send(&mut s, Instant::from_millis(0), &packet);
        assert!(!s.take_urgent_notification());
    }
}
