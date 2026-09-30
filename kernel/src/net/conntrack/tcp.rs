//! TCP protocol state for IPv4 conntrack.
//!
//! The caller has already validated the IP/TCP lengths and checksum. This
//! module owns neither packet storage nor NAT: it only validates a typed TCP
//! segment against both directions' sequence windows and advances state.
//! The transition table and window bounds follow Linux 6.6
//! `nf_conntrack_proto_tcp.c` (without optional SYNPROXY/sequence mangling).

use crate::time::Duration;

const RETRANS_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const UNACK_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const MAX_ACK_WINDOW: u32 = 66_000;
const MAX_RETRANS: u8 = 3;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct TcpOptions {
    /// Parsed from SYN; Linux clamps values above 14.
    pub(crate) window_scale: Option<u8>,
    pub(crate) sack_permitted: bool,
    /// Largest SACK block right edge from this segment, if present.
    pub(crate) highest_sack: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct TcpSegment {
    pub(crate) seq: u32,
    pub(crate) ack_seq: u32,
    pub(crate) window: u16,
    pub(crate) payload_len: u16,
    pub(crate) syn: bool,
    pub(crate) ack: bool,
    pub(crate) fin: bool,
    pub(crate) rst: bool,
    pub(crate) urg: bool,
    pub(crate) options: TcpOptions,
}

impl TcpSegment {
    pub(crate) fn valid(self) -> bool {
        // PSH/ECE/CWR do not affect conntrack's TCP flag validity matrix.
        // A non-ACK SYN or RST can carry URG; FIN must carry ACK.
        let valid_flags = if self.syn {
            !self.fin && !self.rst && (!self.ack || !self.urg)
        } else if self.rst {
            !self.fin && !self.urg
        } else {
            self.ack
        };
        valid_flags
    }

    fn class(self) -> PacketClass {
        if self.rst {
            PacketClass::Rst
        } else if self.syn {
            if self.ack {
                PacketClass::SynAck
            } else {
                PacketClass::Syn
            }
        } else if self.fin {
            PacketClass::Fin
        } else if self.ack {
            PacketClass::Ack
        } else {
            PacketClass::None
        }
    }

    fn end(self) -> u32 {
        self.seq
            .wrapping_add(u32::from(self.payload_len))
            .wrapping_add(u32::from(self.syn))
            .wrapping_add(u32::from(self.fin))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum State {
    SynSent = 0,
    SynRecv = 1,
    Established = 2,
    FinWait = 3,
    CloseWait = 4,
    LastAck = 5,
    TimeWait = 6,
    Close = 7,
    SynSent2 = 8,
}

impl State {
    fn from_transition(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::SynSent,
            2 => Self::SynRecv,
            3 => Self::Established,
            4 => Self::FinWait,
            5 => Self::CloseWait,
            6 => Self::LastAck,
            7 => Self::TimeWait,
            8 => Self::Close,
            9 => Self::SynSent2,
            _ => return None,
        })
    }

    fn timeout(self) -> Duration {
        match self {
            Self::SynSent | Self::SynSent2 | Self::FinWait | Self::TimeWait => {
                Duration::from_secs(120)
            }
            Self::SynRecv | Self::CloseWait => Duration::from_secs(60),
            Self::Established => Duration::from_secs(5 * 24 * 60 * 60),
            Self::LastAck => Duration::from_secs(30),
            Self::Close => Duration::from_secs(10),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(usize)]
enum PacketClass {
    Syn = 0,
    SynAck = 1,
    Fin = 2,
    Ack = 3,
    Rst = 4,
    None = 5,
}

const INVALID: u8 = 10;
const IGNORE: u8 = 11;

// Columns: NONE, SYN_SENT, SYN_RECV, ESTABLISHED, FIN_WAIT, CLOSE_WAIT,
//          LAST_ACK, TIME_WAIT, CLOSE, SYN_SENT2. Only 1..=9 are live states.
// Linux's NONE column is used by TcpTracker::new(), not by an existing flow.
const TRANSITIONS: [[[u8; 10]; 6]; 2] = [
    [
        [1, 1, IGNORE, IGNORE, IGNORE, IGNORE, IGNORE, 1, 1, 9],
        [
            INVALID, INVALID, 2, INVALID, INVALID, INVALID, INVALID, INVALID, INVALID, 2,
        ],
        [INVALID, INVALID, 4, 4, 6, 6, 6, 7, 8, INVALID],
        [3, INVALID, 3, 3, 5, 5, 7, 7, 8, INVALID],
        [INVALID, 8, 8, 8, 8, 8, 8, 8, 8, 8],
        [INVALID; 10],
    ],
    [
        [
            INVALID, 9, INVALID, INVALID, INVALID, INVALID, INVALID, 1, INVALID, 9,
        ],
        [
            INVALID, 2, IGNORE, IGNORE, IGNORE, IGNORE, IGNORE, IGNORE, IGNORE, 2,
        ],
        [INVALID, INVALID, 4, 4, 6, 6, 6, 7, 8, INVALID],
        [INVALID, IGNORE, 2, 3, 5, 5, 7, 7, 8, IGNORE],
        [INVALID, 8, 8, 8, 8, 8, 8, 8, 8, 8],
        [INVALID; 10],
    ],
];

#[derive(Clone, Copy, Debug, Default)]
struct Peer {
    end: u32,
    max_end: u32,
    max_win: u32,
    max_ack: u32,
    scale: u8,
    scale_offered: bool,
    sack_permitted: bool,
    max_ack_set: bool,
    data_unacknowledged: bool,
    close_init: bool,
    liberal: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TcpVerdict {
    Accepted {
        timeout: Option<Duration>,
        destroy: bool,
    },
    Ignored,
    Invalid,
    Repeat,
}

#[derive(Clone, Debug)]
pub(crate) struct TcpTracker {
    state: State,
    peers: [Peer; 2],
    seen_reply: bool,
    assured: bool,
    last_class: PacketClass,
    last_dir: usize,
    last_seq: u32,
    last_ack: u32,
    last_end: u32,
    last_win: u16,
    retrans: u8,
    last_ignored_syn: Option<(usize, TcpSegment)>,
    simultaneous_open: bool,
}

impl TcpTracker {
    pub(crate) fn new(segment: TcpSegment) -> Option<(Self, Duration)> {
        if !segment.valid() {
            return None;
        }
        let state = match segment.class() {
            PacketClass::Syn => State::SynSent,
            PacketClass::Ack => State::Established, // Linux's default tcp_loose=1.
            _ => return None,
        };
        let mut original = Peer {
            end: segment.end(),
            max_win: u32::from(segment.window).max(1),
            ..Peer::default()
        };
        original.max_end = if state == State::SynSent {
            original.end
        } else {
            // Midstream pickup has no known SYN options or peer history.
            original.liberal = true;
            original.end.wrapping_add(original.max_win)
        };
        if state == State::SynSent {
            original.scale_offered = segment.options.window_scale.is_some();
            original.scale = segment.options.window_scale.unwrap_or(0).min(14);
            original.sack_permitted = segment.options.sack_permitted;
        }
        let mut reply = Peer::default();
        if state == State::Established {
            reply.liberal = true;
        }
        let tracker = Self {
            state,
            peers: [original, reply],
            seen_reply: false,
            assured: false,
            last_class: PacketClass::None,
            last_dir: 0,
            last_seq: segment.seq,
            last_ack: segment.ack_seq,
            last_end: segment.end(),
            last_win: segment.window,
            retrans: 0,
            last_ignored_syn: None,
            simultaneous_open: false,
        };
        let timeout = if state == State::Established {
            UNACK_TIMEOUT
        } else {
            state.timeout()
        };
        Some((tracker, timeout))
    }

    pub(crate) fn seen_reply(&self) -> bool {
        self.seen_reply
    }

    pub(super) fn control_state(&self) -> (u8, bool) {
        // State's internal zero-based representation omits Linux NONE.
        (self.state as u8 + 1, self.assured)
    }

    pub(crate) fn observe(&mut self, segment: TcpSegment, direction: usize) -> TcpVerdict {
        if !segment.valid() || direction > 1 {
            return TcpVerdict::Invalid;
        }
        let class = segment.class();
        let old = self.state;
        let transition = TRANSITIONS[direction][class as usize][old as usize + 1];
        if transition == INVALID {
            return TcpVerdict::Invalid;
        }
        if transition == IGNORE {
            if class == PacketClass::SynAck {
                if let Some((syn_dir, ignored_syn)) = self.last_ignored_syn {
                    if syn_dir != direction && segment.ack_seq == ignored_syn.end() {
                        // Linux resynchronizes on a SYN/ACK acknowledging a
                        // previously ignored SYN. Validate the reply against
                        // that SYN before publishing the replacement state.
                        let mut resynced = self.clone();
                        resynced.state = State::SynSent;
                        resynced.peers = [Peer::default(), Peer::default()];
                        let sender = &mut resynced.peers[syn_dir];
                        sender.end = ignored_syn.end();
                        sender.max_end = sender.end;
                        sender.max_win = u32::from(ignored_syn.window).max(1);
                        sender.scale_offered = ignored_syn.options.window_scale.is_some();
                        sender.scale = ignored_syn.options.window_scale.unwrap_or(0).min(14);
                        sender.sack_permitted = ignored_syn.options.sack_permitted;
                        resynced.last_ignored_syn = None;
                        let verdict = resynced.observe(segment, direction);
                        if matches!(verdict, TcpVerdict::Accepted { .. }) {
                            *self = resynced;
                            return verdict;
                        }
                    }
                }
            }
            self.remember(segment, class, direction);
            return TcpVerdict::Ignored;
        }
        let mut new_state = State::from_transition(transition).expect("live TCP transition");
        if new_state == State::SynSent && matches!(old, State::TimeWait | State::Close) {
            if self.peers.iter().any(|peer| peer.close_init)
                || (self.last_dir == direction && self.last_class == PacketClass::Rst)
            {
                return TcpVerdict::Repeat;
            }
            self.remember(segment, class, direction);
            return TcpVerdict::Ignored;
        }
        if new_state == State::SynRecv
            && direction == 1
            && class == PacketClass::Ack
            && self.simultaneous_open
        {
            new_state = State::Established;
        }
        let mut skip_window_check = false;
        if new_state == State::Close && class == PacketClass::Rst {
            // Linux permits early teardown of already-closing flows; their
            // sequence history may belong to a previous incarnation.
            if matches!(
                old,
                State::FinWait | State::CloseWait | State::LastAck | State::TimeWait | State::Close
            ) {
                skip_window_check = true;
            }
            // RFC 5961: a merely in-window RST cannot close an assured
            // established connection unless its SEQ matches a known ACK edge.
            let receiver = self.peers[1 - direction];
            let established = old == State::Established && self.assured;
            if !skip_window_check && receiver.max_ack_set && self.last_class != PacketClass::Syn {
                if segment.seq == 0 && !established {
                    skip_window_check = true;
                } else {
                    if before(segment.seq, receiver.max_ack) {
                        return TcpVerdict::Invalid;
                    }
                    if !established
                        || segment.seq == receiver.max_ack
                        || (self.last_dir == direction
                            && self.last_class == PacketClass::Ack
                            && segment.seq == self.last_end)
                    {
                        skip_window_check = true;
                    } else {
                        new_state = old;
                    }
                }
            }
        }
        let peers = if skip_window_check {
            self.peers
        } else {
            match self.in_window(segment, direction) {
                Ok(peers) => peers,
                Err(verdict) => return verdict,
            }
        };
        self.peers = peers;
        self.state = new_state;
        if new_state == State::SynSent2 {
            self.simultaneous_open = true;
        }
        if old != new_state && new_state == State::FinWait {
            self.peers[direction].close_init = true;
        }
        let previously_seen_reply = self.seen_reply;
        if direction == 1 {
            self.seen_reply = true;
        }
        if self.seen_reply
            && new_state == State::Established
            && matches!(old, State::SynRecv | State::Established)
        {
            self.assured = true;
        }
        let repeated_opening_syn =
            !previously_seen_reply && old == State::SynSent && class == PacketClass::Syn;
        let destroy = !previously_seen_reply && class == PacketClass::Rst;
        self.remember(segment, class, direction);
        let timeout = if repeated_opening_syn {
            None
        } else if class == PacketClass::Rst {
            Some(State::Close.timeout())
        } else if self.retrans >= MAX_RETRANS
            || self.last_win == 0
            || self.peers.iter().any(|peer| peer.data_unacknowledged)
        {
            Some(new_state.timeout().min(RETRANS_TIMEOUT))
        } else if !self.seen_reply && new_state == State::Established {
            Some(UNACK_TIMEOUT)
        } else {
            Some(new_state.timeout())
        };
        TcpVerdict::Accepted { timeout, destroy }
    }

    fn remember(&mut self, segment: TcpSegment, class: PacketClass, direction: usize) {
        if class == PacketClass::Ack
            && self.last_dir == direction
            && self.last_seq == segment.seq
            && self.last_ack == segment.ack_seq
            && self.last_end == segment.end()
            && self.last_win == segment.window
        {
            self.retrans = self.retrans.saturating_add(1);
        } else {
            self.retrans = 0;
        }
        self.last_class = class;
        self.last_dir = direction;
        self.last_seq = segment.seq;
        self.last_ack = segment.ack_seq;
        self.last_end = segment.end();
        self.last_win = segment.window;
        self.last_ignored_syn = if class == PacketClass::Syn && direction == 0 {
            Some((direction, segment))
        } else {
            None
        };
    }

    fn in_window(&self, segment: TcpSegment, direction: usize) -> Result<[Peer; 2], TcpVerdict> {
        let mut peers = self.peers;
        let (sender, receiver) = if direction == 0 {
            let (left, right) = peers.split_at_mut(1);
            (&mut left[0], &mut right[0])
        } else {
            let (left, right) = peers.split_at_mut(1);
            (&mut right[0], &mut left[0])
        };
        let mut seq = segment.seq;
        let mut end = segment.end();
        let mut ack = segment.ack_seq;
        let mut sack = if receiver.sack_permitted {
            segment
                .options
                .highest_sack
                .filter(|right| after(*right, ack))
                .unwrap_or(ack)
        } else {
            ack
        };
        let mut win = u32::from(segment.window);
        if sender.max_win == 0 {
            sender.end = end;
            sender.max_win = win.max(1);
            if segment.syn {
                sender.max_end = end;
                sender.scale_offered = segment.options.window_scale.is_some();
                sender.scale = segment.options.window_scale.unwrap_or(0).min(14);
                sender.sack_permitted = segment.options.sack_permitted;
                if direction == 1 && !(sender.scale_offered && receiver.scale_offered) {
                    sender.scale = 0;
                    receiver.scale = 0;
                }
                if !segment.ack {
                    return Ok(peers);
                }
            } else {
                let scaled = win << sender.scale;
                sender.max_win = scaled.max(1);
                sender.max_end = end.wrapping_add(sender.max_win);
                if receiver.max_win == 0 {
                    receiver.end = sack;
                    receiver.max_end = sack;
                } else if sack == receiver.end.wrapping_add(1) {
                    receiver.end = receiver.end.wrapping_add(1);
                }
            }
        } else if segment.syn
            && after(end, sender.end)
            && matches!(self.state, State::SynSent | State::SynRecv)
        {
            // RFC 793 permits a newer initial sequence during the opening
            // handshake. Linux reinitializes this direction's window here.
            sender.end = end;
            sender.max_end = end;
            sender.max_win = win.max(1);
            sender.scale_offered = segment.options.window_scale.is_some();
            sender.scale = segment.options.window_scale.unwrap_or(0).min(14);
            sender.sack_permitted = segment.options.sack_permitted;
            if direction == 1 && !(sender.scale_offered && receiver.scale_offered) {
                sender.scale = 0;
                receiver.scale = 0;
            }
            if direction == 1 && !segment.ack {
                return Ok(peers);
            }
        }
        if !segment.ack || (segment.rst && ack == 0) {
            ack = receiver.end;
            sack = receiver.end;
        }
        if segment.rst && seq == 0 && self.state == State::SynSent {
            seq = sender.end;
            end = sender.end;
        }
        let max_ack_window = sender.max_win.max(MAX_ACK_WINDOW);
        if !before(seq, sender.max_end.wrapping_add(1)) {
            let overshot = end.wrapping_sub(sender.max_end).wrapping_add(1);
            let ack_ok = after(
                sack,
                receiver
                    .end
                    .wrapping_sub(sender.max_win.max(MAX_ACK_WINDOW))
                    .wrapping_sub(1),
            );
            let in_recv_win = receiver.max_win != 0
                && after(
                    end,
                    sender.end.wrapping_sub(receiver.max_win).wrapping_sub(1),
                );
            if in_recv_win
                && ack_ok
                && overshot <= receiver.max_win
                && before(sack, receiver.end.wrapping_add(1))
            {
                sender.end = end;
                sender.data_unacknowledged = true;
                return if sender.liberal {
                    Ok(peers)
                } else {
                    Err(TcpVerdict::Ignored)
                };
            }
            return if sender.liberal {
                Ok(peers)
            } else {
                Err(TcpVerdict::Invalid)
            };
        }
        if !before(sack, receiver.end.wrapping_add(1)) {
            return if sender.liberal {
                Ok(peers)
            } else {
                Err(TcpVerdict::Invalid)
            };
        }
        if receiver.max_win != 0
            && !after(
                end,
                sender.end.wrapping_sub(receiver.max_win).wrapping_sub(1),
            )
        {
            return if sender.liberal {
                Ok(peers)
            } else {
                Err(TcpVerdict::Ignored)
            };
        }
        if !after(
            sack,
            receiver.end.wrapping_sub(max_ack_window).wrapping_sub(1),
        ) {
            return if sender.liberal {
                Ok(peers)
            } else {
                Err(TcpVerdict::Ignored)
            };
        }
        if !segment.syn {
            win <<= sender.scale;
        }
        sender.max_win = sender.max_win.max(win.wrapping_add(sack.wrapping_sub(ack)));
        if after(end, sender.end) {
            sender.end = end;
            sender.data_unacknowledged = true;
        }
        if segment.ack && (!sender.max_ack_set || after(ack, sender.max_ack)) {
            sender.max_ack = ack;
            sender.max_ack_set = true;
        }
        if receiver.max_win != 0 && after(end, sender.max_end) {
            receiver.max_win = receiver
                .max_win
                .wrapping_add(end.wrapping_sub(sender.max_end));
        }
        let advertised_end = sack.wrapping_add(win.max(1));
        if after(advertised_end, receiver.max_end.wrapping_sub(1)) {
            receiver.max_end = advertised_end;
        }
        if ack == receiver.end {
            receiver.data_unacknowledged = false;
        }
        Ok(peers)
    }
}

fn before(a: u32, b: u32) -> bool {
    (a.wrapping_sub(b) as i32) < 0
}

fn after(a: u32, b: u32) -> bool {
    before(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segment(seq: u32, ack_seq: u32, syn: bool, ack: bool, fin: bool, rst: bool) -> TcpSegment {
        TcpSegment {
            seq,
            ack_seq,
            window: 4096,
            payload_len: 0,
            syn,
            ack,
            fin,
            rst,
            urg: false,
            options: TcpOptions::default(),
        }
    }

    fn established(client: u32, server: u32) -> TcpTracker {
        let (mut tracker, _) =
            TcpTracker::new(segment(client, 0, true, false, false, false)).unwrap();
        assert!(matches!(
            tracker.observe(
                segment(server, client.wrapping_add(1), true, true, false, false),
                1
            ),
            TcpVerdict::Accepted { .. }
        ));
        assert!(matches!(
            tracker.observe(
                segment(
                    client.wrapping_add(1),
                    server.wrapping_add(1),
                    false,
                    true,
                    false,
                    false
                ),
                0
            ),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::Established);
        tracker
    }

    #[test]
    fn out_of_window_rst_does_not_close_established_flow() {
        let mut tracker = established(100, 500);
        assert_eq!(
            tracker.observe(segment(1_000_000, 101, false, true, false, true), 1),
            TcpVerdict::Invalid
        );
        assert_eq!(tracker.state, State::Established);
        assert!(matches!(
            tracker.observe(segment(501, 101, false, true, false, false), 1),
            TcpVerdict::Accepted { destroy: false, .. }
        ));
    }

    #[test]
    fn in_window_non_exact_rst_keeps_established_state_for_challenge_ack() {
        let mut tracker = established(100, 500);
        assert!(matches!(
            tracker.observe(segment(502, 101, false, true, false, true), 1),
            TcpVerdict::Accepted { destroy: false, .. }
        ));
        assert_eq!(tracker.state, State::Established);
    }

    #[test]
    fn old_ack_edge_rst_is_invalid_even_if_inside_advertised_window() {
        let mut tracker = established(100, 500);
        assert_eq!(
            tracker.observe(segment(500, 101, false, true, false, true), 1),
            TcpVerdict::Invalid
        );
        assert_eq!(tracker.state, State::Established);
    }

    #[test]
    fn fin_ack_progresses_to_time_wait_and_new_syn_repeats() {
        let mut tracker = established(100, 500);
        assert!(matches!(
            tracker.observe(segment(101, 501, false, true, true, false), 0),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::FinWait);
        assert!(matches!(
            tracker.observe(segment(501, 102, false, true, false, false), 1),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::CloseWait);
        assert!(matches!(
            tracker.observe(segment(501, 102, false, true, true, false), 1),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::LastAck);
        assert!(matches!(
            tracker.observe(segment(102, 502, false, true, false, false), 0),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::TimeWait);
        assert_eq!(
            tracker.observe(segment(900, 0, true, false, false, false), 0),
            TcpVerdict::Repeat
        );
    }

    #[test]
    fn syn_retransmit_does_not_refresh_timeout_and_reply_rst_destroys() {
        let (mut tracker, timeout) =
            TcpTracker::new(segment(100, 0, true, false, false, false)).unwrap();
        assert_eq!(timeout, Duration::from_secs(120));
        assert_eq!(
            tracker.observe(segment(100, 0, true, false, false, false), 0),
            TcpVerdict::Accepted {
                timeout: None,
                destroy: false
            }
        );
        assert!(matches!(
            tracker.observe(segment(0, 101, false, true, false, true), 1),
            TcpVerdict::Accepted { destroy: true, .. }
        ));
    }

    #[test]
    fn newer_syn_sequence_reinitializes_opening_window() {
        let (mut tracker, _) = TcpTracker::new(segment(100, 0, true, false, false, false)).unwrap();
        assert_eq!(
            tracker.observe(segment(200, 0, true, false, false, false), 0),
            TcpVerdict::Accepted {
                timeout: None,
                destroy: false
            }
        );
        assert_eq!(tracker.peers[0].end, 201);
        assert!(matches!(
            tracker.observe(segment(500, 201, true, true, false, false), 1),
            TcpVerdict::Accepted { .. }
        ));
    }

    #[test]
    fn sequence_window_handles_wraparound() {
        let mut tracker = established(u32::MAX - 1, 100);
        let mut data = segment(u32::MAX, 101, false, true, false, false);
        data.payload_len = 2;
        assert!(matches!(
            tracker.observe(data, 0),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.peers[0].end, 1);
    }

    #[test]
    fn stale_sack_right_edge_cannot_replace_newer_ack() {
        for (client, server) in [(100, 500), (u32::MAX - 1, 100)] {
            let mut tracker = established(client, server);
            tracker.peers[1].sack_permitted = true;
            let ack = server.wrapping_add(1);
            let normal = segment(client.wrapping_add(1), ack, false, true, false, false);
            let mut stale = normal;
            stale.options.highest_sack = Some(ack.wrapping_sub(1));
            let expected = tracker.in_window(normal, 0).unwrap();
            let observed = tracker.in_window(stale, 0).unwrap();
            assert_eq!(observed[0].max_win, expected[0].max_win);
            assert_eq!(observed[1].max_end, expected[1].max_end);
        }
    }

    #[test]
    fn ignored_syn_is_resynchronized_by_matching_synack() {
        let mut tracker = established(100, 500);
        assert_eq!(
            tracker.observe(segment(900, 0, true, false, false, false), 0),
            TcpVerdict::Ignored
        );
        assert!(matches!(
            tracker.observe(segment(2000, 901, true, true, false, false), 1),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::SynRecv);
        assert!(matches!(
            tracker.observe(segment(901, 2001, false, true, false, false), 0),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::Established);
    }

    #[test]
    fn default_loose_pickup_becomes_assured_after_valid_reply() {
        let (mut tracker, timeout) =
            TcpTracker::new(segment(100, 300, false, true, false, false)).unwrap();
        assert_eq!(timeout, UNACK_TIMEOUT);
        assert!(!tracker.assured);
        assert!(matches!(
            tracker.observe(segment(300, 100, false, true, false, false), 1),
            TcpVerdict::Accepted { .. }
        ));
        assert!(tracker.assured);
        assert!(tracker.seen_reply());
    }

    #[test]
    fn simultaneous_open_reaches_established_after_reply_ack() {
        let (mut tracker, _) = TcpTracker::new(segment(100, 0, true, false, false, false)).unwrap();
        assert!(matches!(
            tracker.observe(segment(500, 0, true, false, false, false), 1),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::SynSent2);
        assert!(matches!(
            tracker.observe(segment(100, 501, true, true, false, false), 0),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::SynRecv);
        assert!(matches!(
            tracker.observe(segment(501, 101, false, true, false, false), 1),
            TcpVerdict::Accepted { .. }
        ));
        assert_eq!(tracker.state, State::Established);
    }

    #[test]
    fn malformed_flag_combinations_are_not_trackable() {
        let mut invalid = segment(1, 0, false, false, false, false);
        assert!(TcpTracker::new(invalid).is_none());
        invalid.syn = true;
        invalid.rst = true;
        assert!(TcpTracker::new(invalid).is_none());
        invalid.rst = false;
        invalid.payload_len = 4; // TCP Fast Open is valid.
        assert!(TcpTracker::new(invalid).is_some());
    }
}
