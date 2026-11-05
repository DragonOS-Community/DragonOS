//! Compact protocol state shared by attached and detached TIME-WAIT sockets.
use super::*;

#[derive(Debug, Clone)]
pub(crate) struct TimeWaitState {
    pub tuple: Tuple,
    pub device: u32,
    pub expires: Instant,
    pub ts_recent: Option<(u32, Instant)>,
    duration: Duration,
    snd_nxt: TcpSeqNumber,
    rcv_nxt: TcpSeqNumber,
    window: u16,
    hop_limit: u8,
    ts_generator: Option<TcpTimestampGenerator>,
    ack_at: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TimeWaitAction {
    Ignore,
    Ack,
    Reopen,
    Remove,
}

pub(super) fn paws_reject(
    recent: Option<(u32, Instant)>,
    repr: &TcpRepr<'_>,
    now: Instant,
) -> bool {
    match (recent, repr.timestamp) {
        (Some((recent, stamp)), Some(ts)) => {
            recent != 0
                && (ts.tsval.wrapping_sub(recent) as i32) < 0
                && now - stamp < Duration::from_secs(24 * 24 * 60 * 60)
                && (repr.control != TcpControl::Rst || now - stamp < Duration::from_secs(60))
        }
        _ => false,
    }
}

impl TimeWaitState {
    pub fn from_socket(socket: &Socket<'_>, now: Instant) -> Self {
        Self {
            tuple: socket.tuple.unwrap(),
            device: socket.lifecycle_device(),
            expires: now + socket.time_wait_duration,
            duration: socket.time_wait_duration,
            snd_nxt: socket.local_seq_no,
            rcv_nxt: socket.remote_seq_no + socket.rx_buffer.len(),
            window: socket.scaled_window(),
            hop_limit: socket.hop_limit.unwrap_or(64),
            ts_recent: socket.ts_recent,
            ts_generator: socket.tsval_generator,
            ack_at: Instant::from_millis(0),
        }
    }

    pub fn new_isn(&self) -> TcpSeqNumber {
        let seq = TcpSeqNumber(self.snd_nxt.0.wrapping_add(65537));
        if seq == TcpSeqNumber(0) {
            TcpSeqNumber(1)
        } else {
            seq
        }
    }

    /// Relaxing the receive sequence cutoff is safe only if the new connection
    /// continues negotiating timestamps; otherwise delayed old segments lose
    /// the PAWS protection on which timestamp-only reopening depends.
    pub fn can_reopen_on(&self, repr: &TcpRepr<'_>, listener: &Socket<'_>) -> bool {
        repr.seq_number > self.rcv_nxt || listener.tsval_generator.is_some()
    }

    pub fn active_reuse(&self, now: Instant, mode: TimeWaitReuse) -> bool {
        let Some((_, stamp)) = self.ts_recent else {
            return false;
        };
        if self.ts_generator.is_none() {
            return false;
        }
        match mode {
            TimeWaitReuse::Explicit => true,
            TimeWaitReuse::Automatic { loopback } => {
                loopback && now.total_millis() / 1000 > stamp.total_millis() / 1000
            }
        }
    }

    pub fn process(&mut self, now: Instant, repr: &TcpRepr<'_>) -> TimeWaitAction {
        if now >= self.expires {
            return TimeWaitAction::Remove;
        }
        let paws = paws_reject(self.ts_recent, repr, now);
        if !paws
            && repr.seq_number == self.rcv_nxt
            && (repr.segment_len() == 0 || repr.control == TcpControl::Rst)
        {
            if repr.control == TcpControl::Rst {
                return TimeWaitAction::Remove;
            }
            self.expires = now + self.duration;
            if let Some(ts) = repr.timestamp {
                self.ts_recent = Some((ts.tsval, now));
            }
            return TimeWaitAction::Ignore;
        }
        let newer_timestamp = match (self.ts_recent, repr.timestamp) {
            (Some((recent, _)), Some(ts)) => (ts.tsval.wrapping_sub(recent) as i32) > 0,
            _ => false,
        };
        if repr.control == TcpControl::Syn
            && repr.ack_number.is_none()
            && !paws
            && (repr.seq_number > self.rcv_nxt || newer_timestamp)
        {
            return TimeWaitAction::Reopen;
        }
        if repr.control == TcpControl::Rst {
            return TimeWaitAction::Ignore;
        }
        if paws || repr.ack_number.is_some() {
            self.expires = now + self.duration;
        }
        TimeWaitAction::Ack
    }

    pub fn reply(&mut self, now: Instant, _: &TcpRepr<'_>) -> Option<(IpRepr, TcpRepr<'static>)> {
        if now < self.ack_at {
            return None;
        }
        self.ack_at = now + Duration::from_millis(500);
        let tcp = TcpRepr {
            src_port: self.tuple.local.port,
            dst_port: self.tuple.remote.port,
            control: TcpControl::None,
            seq_number: self.snd_nxt,
            ack_number: Some(self.rcv_nxt),
            urgent_at: None,
            window_len: self.window,
            window_scale: None,
            max_seg_size: None,
            sack_permitted: false,
            sack_ranges: [None, None, None],
            timestamp: self.ts_generator.map(|generator| TcpTimestampRepr {
                tsval: generator(),
                tsecr: self.ts_recent.map_or(0, |t| t.0),
            }),
            payload: &[],
        };
        Some((
            IpRepr::new(
                self.tuple.local.addr,
                self.tuple.remote.addr,
                IpProtocol::Tcp,
                tcp.buffer_len(),
                self.hop_limit,
            ),
            tcp,
        ))
    }

    pub fn meta(&self) -> PacketMeta {
        let mut meta = PacketMeta::default();
        #[cfg(feature = "packetmeta-id")]
        {
            meta.id = self.device;
        }
        meta
    }
}

#[cfg(all(test, feature = "alloc", feature = "medium-ip", feature = "proto-ipv4"))]
mod tests {
    use super::*;
    use crate::iface::SocketSet;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    fn endpoints() -> (IpEndpoint, IpEndpoint) {
        (
            IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 80),
            IpEndpoint::new(IpAddress::v4(127, 0, 0, 1), 40000),
        )
    }
    fn empty() -> Socket<'static> {
        Socket::new(
            SocketBuffer::new(vec![0; 64]),
            SocketBuffer::new(vec![0; 64]),
        )
    }
    fn old() -> Socket<'static> {
        let (local, remote) = endpoints();
        let mut s = empty();
        s.state = State::TimeWait;
        s.tuple = Some(Tuple { local, remote });
        s.local_seq_no = TcpSeqNumber(1000);
        s.remote_seq_no = TcpSeqNumber(2000);
        s.remote_last_ack = Some(s.remote_seq_no);
        s.set_time_wait_duration(Duration::from_secs(60));
        s.set_tsval_generator(Some(|| 5000));
        s.ts_recent = Some((3000, Instant::from_secs(1)));
        s.ensure_time_wait(Instant::from_secs(1));
        s
    }
    fn syn(seq: i32, ts: u32) -> TcpRepr<'static> {
        let (local, remote) = endpoints();
        TcpRepr {
            src_port: remote.port,
            dst_port: local.port,
            control: TcpControl::Syn,
            seq_number: TcpSeqNumber(seq),
            ack_number: None,
            urgent_at: None,
            window_len: 1000,
            window_scale: None,
            max_seg_size: Some(1460),
            sack_permitted: false,
            sack_ranges: [None; 3],
            timestamp: Some(TcpTimestampRepr {
                tsval: ts,
                tsecr: 0,
            }),
            payload: &[],
        }
    }
    fn ip() -> IpRepr {
        let (l, r) = endpoints();
        IpRepr::new(r.addr, l.addr, IpProtocol::Tcp, 40, 64)
    }
    fn listener() -> Socket<'static> {
        let mut s = empty();
        s.listen(endpoints().0).unwrap();
        s
    }
    fn context() -> Context {
        crate::tests::setup(crate::phy::Medium::Ip).0.inner
    }

    #[test]
    fn old_syn_paws_and_sequence_wrap() {
        let mut tw = old().time_wait.take().unwrap();
        assert_eq!(
            tw.process(Instant::from_secs(2), &syn(2100, 2999)),
            TimeWaitAction::Ack
        );
        assert_eq!(
            tw.process(Instant::from_secs(2), &syn(1999, 3000)),
            TimeWaitAction::Ack
        );
        assert_eq!(
            tw.process(Instant::from_secs(2), &syn(2100, 3000)),
            TimeWaitAction::Reopen
        );
        assert_eq!(
            tw.process(Instant::from_secs(2), &syn(1999, 3001)),
            TimeWaitAction::Reopen
        );
        tw.rcv_nxt = TcpSeqNumber(i32::MAX - 1);
        assert_eq!(
            tw.process(Instant::from_secs(2), &syn(i32::MIN + 1, 3000)),
            TimeWaitAction::Reopen
        );
        tw.ts_recent = Some((u32::MAX, Instant::from_secs(1)));
        assert_eq!(
            tw.process(Instant::from_secs(2), &syn(i32::MIN, 1)),
            TimeWaitAction::Reopen
        );
    }

    #[test]
    fn reopened_isn_wraps_independently_of_pointer_width() {
        let mut tw = old().time_wait.take().unwrap();
        tw.snd_nxt = TcpSeqNumber(i32::MAX - 65536);
        assert_eq!(tw.new_isn(), TcpSeqNumber(i32::MIN));
        tw.snd_nxt = TcpSeqNumber(-65537);
        assert_eq!(tw.new_isn(), TcpSeqNumber(1));
    }

    #[test]
    fn active_reuse_distinguishes_explicit_and_automatic() {
        let tw = old().time_wait.take().unwrap();
        assert!(tw.active_reuse(Instant::from_millis(1500), TimeWaitReuse::Explicit));
        assert!(!tw.active_reuse(
            Instant::from_millis(1500),
            TimeWaitReuse::Automatic { loopback: true }
        ));
        assert!(!tw.active_reuse(
            Instant::from_secs(2),
            TimeWaitReuse::Automatic { loopback: false }
        ));
        assert!(tw.active_reuse(
            Instant::from_secs(2),
            TimeWaitReuse::Automatic { loopback: true }
        ));
    }

    #[test]
    fn fin_ack_rst_and_rate_limit() {
        let mut tw = old().time_wait.take().unwrap();
        let mut fin = syn(1999, 3000);
        fin.control = TcpControl::Fin;
        fin.ack_number = Some(TcpSeqNumber(1000));
        let now = Instant::from_secs(5);
        assert_eq!(tw.process(now, &fin), TimeWaitAction::Ack);
        let reply = tw.reply(now, &fin).unwrap().1;
        assert_eq!(reply.ack_number, Some(TcpSeqNumber(2000)));
        assert_eq!(tw.expires, Instant::from_secs(65));
        assert!(tw.reply(now, &fin).is_none());
        assert!(tw.reply(now + Duration::from_millis(500), &fin).is_some());
        let mut ack = fin;
        ack.control = TcpControl::None;
        ack.seq_number = TcpSeqNumber(2000);
        assert_eq!(
            tw.process(Instant::from_secs(6), &ack),
            TimeWaitAction::Ignore
        );
        assert_eq!(tw.expires, Instant::from_secs(66));
        ack.control = TcpControl::Rst;
        ack.seq_number = TcpSeqNumber(1990);
        assert_eq!(tw.process(now, &ack), TimeWaitAction::Ignore);
        ack.seq_number = TcpSeqNumber(2000);
        assert_eq!(tw.process(now, &ack), TimeWaitAction::Remove);
    }

    #[test]
    fn compact_and_attached_reopen_ignore_slot_order() {
        for compact in [false, true] {
            for listener_first in [false, true] {
                let mut sockets = SocketSet::new(vec![]);
                let (old_handle, listen_handle) = if listener_first {
                    let l = sockets.add(listener());
                    (sockets.add(old()), l)
                } else {
                    let o = sockets.add(old());
                    (o, sockets.add(listener()))
                };
                if compact {
                    assert!(sockets.detach_tcp_time_wait(old_handle));
                }
                let mut cx = context();
                cx.set_now(Instant::from_secs(2));
                assert!(sockets
                    .process_tcp_sockets(&mut cx, PacketMeta::default(), &ip(), &syn(2100, 3001))
                    .is_some());
                let l = sockets.get::<Socket>(listen_handle);
                assert_eq!(l.state(), State::SynReceived);
                assert_eq!(l.local_seq_no, TcpSeqNumber(66537));
                assert!(!sockets.has_tcp_time_wait());
                if !compact {
                    assert_eq!(sockets.get::<Socket>(old_handle).state(), State::Closed);
                }
            }
        }
    }

    #[test]
    fn missing_or_invalid_listener_preserves_protection() {
        for invalid_mss in [false, true] {
            let mut sockets = SocketSet::new(vec![]);
            let h = sockets.add(old());
            assert!(sockets.detach_tcp_time_wait(h));
            if invalid_mss {
                sockets.add(listener());
            }
            let mut cx = context();
            cx.set_now(Instant::from_secs(2));
            let mut request = syn(2100, 3001);
            if invalid_mss {
                request.max_seg_size = Some(0);
            }
            assert!(sockets
                .process_tcp_sockets(&mut cx, PacketMeta::default(), &ip(), &request)
                .unwrap()
                .is_some());
            assert!(sockets.has_tcp_time_wait());
        }
    }

    #[test]
    fn stale_syn_never_reaches_listener() {
        let mut sockets = SocketSet::new(vec![]);
        let l = sockets.add(listener());
        let h = sockets.add(old());
        assert!(sockets.detach_tcp_time_wait(h));
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        sockets.process_tcp_sockets(&mut cx, PacketMeta::default(), &ip(), &syn(2100, 2999));
        assert_eq!(sockets.get::<Socket>(l).state(), State::Listen);
        assert!(sockets.has_tcp_time_wait());
    }

    #[derive(Debug)]
    struct Observer {
        closed: Arc<AtomicUsize>,
        reject: bool,
    }
    impl LifecycleObserver for Observer {
        fn identity(&self) -> u64 {
            9
        }
        fn prepare_open(&self, _: IpEndpoint, _: IpEndpoint, _: u32, _: Option<u64>) -> bool {
            !self.reject
        }
        fn on_state_change(
            &self,
            state: State,
            _: Option<IpEndpoint>,
            _: Option<IpEndpoint>,
            _: u32,
        ) {
            if state == State::Closed {
                self.closed.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    #[test]
    fn expiry_releases_observer_once_and_deadline_refresh_is_bounded() {
        let count = Arc::new(AtomicUsize::new(0));
        let mut s = old();
        s.set_lifecycle_observer(Some(Arc::new(Observer {
            closed: count.clone(),
            reject: false,
        })));
        let mut sockets = SocketSet::new(vec![]);
        let h = sockets.add(s);
        assert!(sockets.detach_tcp_time_wait(h));
        let mut cx = context();
        let mut ack = syn(2000, 3001);
        ack.control = TcpControl::None;
        ack.ack_number = Some(TcpSeqNumber(1000));
        for sec in 2..100 {
            cx.set_now(Instant::from_secs(sec));
            sockets.process_tcp_sockets(&mut cx, PacketMeta::default(), &ip(), &ack);
        }
        assert_eq!(
            sockets.tcp_time_wait_poll_at(),
            Some(Instant::from_secs(159))
        );
        sockets.expire_tcp_time_wait(Instant::from_secs(158));
        assert!(sockets.has_tcp_time_wait());
        sockets.expire_tcp_time_wait(Instant::from_secs(159));
        assert!(!sockets.has_tcp_time_wait());
        sockets.expire_tcp_time_wait(Instant::from_secs(200));
        assert_eq!(count.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn active_failure_preserves_both_sockets_then_explicit_reuse_succeeds() {
        let mut sockets = SocketSet::new(vec![]);
        let h = sockets.add(old());
        assert!(sockets.detach_tcp_time_wait(h));
        let mut new = empty();
        new.set_tsval_generator(Some(|| 5000));
        let n = sockets.add(new);
        let (local, remote) = endpoints();
        let mut cx = context();
        cx.set_now(Instant::from_millis(1500));
        assert_eq!(
            sockets.connect_tcp(
                n,
                &mut cx,
                remote,
                local,
                TimeWaitReuse::Automatic { loopback: true }
            ),
            Err(ConnectError::AddressInUse)
        );
        assert_eq!(sockets.get::<Socket>(n).state(), State::Closed);
        assert!(sockets.has_tcp_time_wait());
        sockets
            .connect_tcp(n, &mut cx, remote, local, TimeWaitReuse::Explicit)
            .unwrap();
        assert_eq!(sockets.get::<Socket>(n).local_seq_no, TcpSeqNumber(66537));
        assert!(!sockets.has_tcp_time_wait());
    }

    #[test]
    fn rejected_observer_keeps_time_wait() {
        let mut sockets = SocketSet::new(vec![]);
        let h = sockets.add(old());
        assert!(sockets.detach_tcp_time_wait(h));
        let mut l = listener();
        l.set_lifecycle_observer(Some(Arc::new(Observer {
            closed: Arc::new(AtomicUsize::new(0)),
            reject: true,
        })));
        let l = sockets.add(l);
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        sockets.process_tcp_sockets(&mut cx, PacketMeta::default(), &ip(), &syn(2100, 3001));
        assert_eq!(sockets.get::<Socket>(l).state(), State::Listen);
        assert!(sockets.has_tcp_time_wait());
    }

    #[test]
    fn attached_expiry_preserves_unread_data() {
        let mut s = old();
        assert_eq!(s.rx_buffer.enqueue_slice(b"abc"), 3);
        let mut cx = context();
        cx.set_now(Instant::from_secs(62));
        s.dispatch(&mut cx, |_, _| -> Result<(), ()> {
            panic!("expired TW emitted packet")
        })
        .unwrap();
        assert_eq!(s.state(), State::Closed);
        assert_eq!(s.recv_queue(), 3);
    }

    #[cfg(feature = "packetmeta-id")]
    #[test]
    fn device_isolation_and_wildcard_overlap() {
        let mut sockets = SocketSet::new(vec![]);
        for device in [2, 3] {
            let mut s = old();
            s.connection_bound_device = NonZeroU32::new(device);
            s.time_wait = None;
            s.ensure_time_wait(Instant::from_secs(1));
            let h = sockets.add(s);
            assert!(sockets.detach_tcp_time_wait(h));
        }
        let mut new = empty();
        new.set_tsval_generator(Some(|| 5000));
        let n = sockets.add(new);
        let (local, remote) = endpoints();
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        // An unbound identity overlaps both records and cannot replace just one.
        assert_eq!(
            sockets.connect_tcp(n, &mut cx, remote, local, TimeWaitReuse::Explicit),
            Err(ConnectError::AddressInUse)
        );
        let mut l = listener();
        l.listen_bound_device = NonZeroU32::new(3);
        let l = sockets.add(l);
        sockets.process_tcp_sockets(&mut cx, PacketMeta { id: 2 }, &ip(), &syn(2100, 3001));
        assert_eq!(sockets.get::<Socket>(l).state(), State::Listen);
        sockets.process_tcp_sockets(&mut cx, PacketMeta { id: 3 }, &ip(), &syn(2100, 3001));
        assert_eq!(sockets.get::<Socket>(l).state(), State::SynReceived);
        assert!(sockets.has_tcp_time_wait()); // device 2 remains protected
    }

    #[cfg(feature = "proto-ipv6")]
    #[test]
    fn ipv6_compact_reopen_preserves_family() {
        let local = IpEndpoint::new(IpAddress::Ipv6(crate::wire::Ipv6Address::LOCALHOST), 80);
        let remote = IpEndpoint::new(local.addr, 40000);
        let mut s = old();
        s.tuple = Some(Tuple { local, remote });
        s.time_wait = None;
        s.ensure_time_wait(Instant::from_secs(1));
        let mut sockets = SocketSet::new(vec![]);
        let h = sockets.add(s);
        assert!(sockets.detach_tcp_time_wait(h));
        let mut l = empty();
        l.listen(local).unwrap();
        let l = sockets.add(l);
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        let packet = IpRepr::new(remote.addr, local.addr, IpProtocol::Tcp, 40, 64);
        sockets.process_tcp_sockets(&mut cx, PacketMeta::default(), &packet, &syn(2100, 3001));
        assert_eq!(sockets.get::<Socket>(l).local_endpoint(), Some(local));
        assert_eq!(sockets.get::<Socket>(l).remote_endpoint(), Some(remote));
        assert_eq!(sockets.get::<Socket>(l).state(), State::SynReceived);
        assert!(!sockets.has_tcp_time_wait());
    }

    #[test]
    fn paws_ignores_zero_recent_and_relaxes_rst_after_msl() {
        let mut packet = syn(2000, 2999);
        packet.control = TcpControl::Rst;
        assert!(paws_reject(
            Some((3000, Instant::from_secs(1))),
            &packet,
            Instant::from_secs(2)
        ));
        assert!(!paws_reject(
            Some((3000, Instant::from_secs(1))),
            &packet,
            Instant::from_secs(61)
        ));
        assert!(!paws_reject(
            Some((0, Instant::from_secs(1))),
            &syn(2000, u32::MAX),
            Instant::from_secs(2)
        ));
    }

    #[test]
    fn reset_handshake_does_not_poison_listener_timestamp_clock() {
        let mut s = listener();
        s.set_tsval_generator(Some(|| 5000));
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        s.process(&mut cx, &ip(), &syn(100, 1_000_000));
        assert_eq!(s.state(), State::SynReceived);
        assert_eq!(s.ts_recent.map(|t| t.0), Some(1_000_000));
        // A transmitted SYN-ACK leaves a receive-sequence reference behind.
        s.remote_last_ack = Some(TcpSeqNumber(101));
        let mut rst = syn(101, 0);
        rst.control = TcpControl::Rst;
        rst.timestamp = None;
        s.process(&mut cx, &ip(), &rst);
        assert_eq!(s.state(), State::Listen);
        assert_eq!(s.ts_recent, None);
        assert_eq!(s.remote_last_ack, None);
        assert_eq!(s.last_remote_tsval, 0);
        // A different peer is free to have a lower clock and a larger ISN.
        s.process(&mut cx, &ip(), &syn(10000, 100));
        assert_eq!(s.state(), State::SynReceived);
        assert_eq!(s.ts_recent.map(|t| t.0), Some(100));
    }

    #[test]
    fn timestamp_only_reopen_requires_new_listener_timestamp_support() {
        for compact in [false, true] {
            for timestamps in [false, true] {
                for newer_sequence in [false, true] {
                    let mut sockets = SocketSet::new(vec![]);
                    let h = sockets.add(old());
                    if compact {
                        assert!(sockets.detach_tcp_time_wait(h));
                    }
                    let mut l = listener();
                    if timestamps {
                        l.set_tsval_generator(Some(|| 5000));
                    }
                    let l = sockets.add(l);
                    let mut cx = context();
                    cx.set_now(Instant::from_secs(2));
                    let seq = if newer_sequence { 2100 } else { 1999 };
                    let result = sockets.process_tcp_sockets(
                        &mut cx,
                        PacketMeta::default(),
                        &ip(),
                        &syn(seq, 3001),
                    );
                    if timestamps || newer_sequence {
                        assert_eq!(sockets.get::<Socket>(l).state(), State::SynReceived);
                        assert!(!sockets.has_tcp_time_wait());
                        if !compact {
                            assert_eq!(sockets.get::<Socket>(h).state(), State::Closed);
                        }
                        if timestamps {
                            assert_eq!(sockets.get::<Socket>(l).ts_recent.map(|t| t.0), Some(3001));
                        }
                    } else {
                        assert_eq!(sockets.get::<Socket>(l).state(), State::Listen);
                        assert!(result.unwrap().is_some(), "old TW must ACK unsafe reopen");
                        if compact {
                            assert!(sockets.has_tcp_time_wait());
                        } else {
                            assert_eq!(sockets.get::<Socket>(h).state(), State::TimeWait);
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn timestamp_configuration_survives_rejected_non_timestamp_handshake() {
        for relisten in [false, true] {
            let mut s = listener();
            s.set_tsval_generator(Some(|| 5000));
            let mut cx = context();
            cx.set_now(Instant::from_secs(2));
            let mut request = syn(100, 0);
            request.timestamp = None;
            s.process(&mut cx, &ip(), &request);
            assert_eq!(s.state(), State::SynReceived);
            assert!(!s.timestamp_enabled());
            let mut rst = request;
            rst.control = TcpControl::Rst;
            rst.seq_number = TcpSeqNumber(101);
            s.process(&mut cx, &ip(), &rst);
            assert_eq!(s.state(), State::Listen);
            if relisten {
                s.listen(endpoints().0).unwrap();
            }
            assert!(s.timestamp_enabled());
            s.process(&mut cx, &ip(), &syn(1000, 100));
            assert_eq!(s.state(), State::SynReceived);
            assert!(s.timestamp_enabled());
            assert_eq!(s.ts_recent.map(|t| t.0), Some(100));
        }
    }

    #[test]
    fn timestamp_configuration_survives_active_reset_and_retry() {
        let mut s = empty();
        s.set_tsval_generator(Some(|| 5000));
        let (local, remote) = endpoints();
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        s.connect(&mut cx, remote, local).unwrap();
        s.dispatch(&mut cx, |_, _| -> Result<(), ()> { Ok(()) })
            .unwrap();
        let mut reply = syn(100, 0);
        reply.timestamp = None;
        reply.ack_number = Some(s.local_seq_no + 1);
        s.process(&mut cx, &ip(), &reply);
        assert_eq!(s.state(), State::Established);
        assert!(!s.timestamp_enabled());
        let mut rst = reply;
        rst.control = TcpControl::Rst;
        rst.seq_number = TcpSeqNumber(101);
        s.process(&mut cx, &ip(), &rst);
        assert_eq!(s.state(), State::Closed);
        s.connect(&mut cx, remote, local).unwrap();
        assert!(s.timestamp_enabled());
        assert_eq!(s.ts_recent, None);
        s.dispatch(&mut cx, |_, _| -> Result<(), ()> { Ok(()) })
            .unwrap();
        let mut reply = syn(1000, 100);
        reply.ack_number = Some(s.local_seq_no + 1);
        s.process(&mut cx, &ip(), &reply);
        assert_eq!(s.state(), State::Established);
        assert!(s.timestamp_enabled());
        assert_eq!(s.ts_recent.map(|t| t.0), Some(100));
    }

    #[test]
    fn active_reuse_checks_timestamp_configuration_before_reset() {
        let mut sockets = SocketSet::new(vec![]);
        let h = sockets.add(old());
        assert!(sockets.detach_tcp_time_wait(h));
        let mut new = empty();
        new.set_tsval_generator(Some(|| 5000));
        // A previous peer declined timestamps; a fresh connect renegotiates.
        new.tsval_generator = None;
        let n = sockets.add(new);
        let (local, remote) = endpoints();
        let mut cx = context();
        cx.set_now(Instant::from_secs(2));
        sockets
            .connect_tcp(n, &mut cx, remote, local, TimeWaitReuse::Explicit)
            .unwrap();
        assert!(sockets.get::<Socket>(n).timestamp_enabled());
        assert!(!sockets.has_tcp_time_wait());
    }
}
