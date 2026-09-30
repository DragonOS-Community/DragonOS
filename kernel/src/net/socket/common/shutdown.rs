use crate::filesystem::epoll::EPollEventType;

/// Shutdown bit for socket operations.
///
/// Mirrors Linux `struct sock::sk_shutdown`: the bits are sticky (a later
/// `shutdown(2)` only adds to them) and both the connected and the listening
/// form of an AF_UNIX socket derive their poll state from them.
pub struct ShutdownBit {
    bit: u8,
}

impl ShutdownBit {
    const RCV_SHUTDOWN: u8 = 0x01;
    const SEND_SHUTDOWN: u8 = 0x02;
    const SHUTDOWN_MASK: u8 = 0x03;

    /// Neither direction shut down yet.
    pub const NONE: ShutdownBit = ShutdownBit { bit: 0 };

    // Public constants for callers, mirroring the Linux/POSIX shutdown(2) semantics.
    pub const SHUT_RD: ShutdownBit = ShutdownBit {
        bit: Self::RCV_SHUTDOWN,
    };
    pub const SHUT_WR: ShutdownBit = ShutdownBit {
        bit: Self::SEND_SHUTDOWN,
    };
    pub const SHUT_RDWR: ShutdownBit = ShutdownBit {
        bit: Self::RCV_SHUTDOWN | Self::SEND_SHUTDOWN,
    };

    /// The raw internal bit mask.
    #[inline]
    pub fn bits(&self) -> u8 {
        self.bit
    }

    /// Build a `ShutdownBit` from a raw integer, discarding invalid bits.
    ///
    /// `raw` holds the internal state bits (`RCV`/`SEND`), not the `how`
    /// argument of `shutdown(2)`.
    #[inline]
    pub fn from_bits_truncate(raw: usize) -> ShutdownBit {
        ShutdownBit {
            bit: (raw as u8) & Self::SHUTDOWN_MASK,
        }
    }

    /// Build the mask from the per-direction "shut down" flags.
    ///
    /// A connected socket keeps its two directions in different places (receive
    /// ring and send state); this constructor folds them into a single mask so
    /// both forms can share [`Self::poll_events`].
    #[inline]
    pub fn from_flags(recv_shutdown: bool, send_shutdown: bool) -> ShutdownBit {
        let mut bit = 0;
        if recv_shutdown {
            bit |= Self::RCV_SHUTDOWN;
        }
        if send_shutdown {
            bit |= Self::SEND_SHUTDOWN;
        }
        ShutdownBit { bit }
    }

    /// The `poll`/`epoll` contribution of the latched shutdown bits, as in
    /// Linux `unix_poll()`.
    ///
    /// - `RCV_SHUTDOWN`: readable plus half-close (`EPOLLIN | EPOLLRDNORM |
    ///   EPOLLRDHUP`);
    /// - both directions shut down (`SHUTDOWN_MASK`): adds `EPOLLHUP`.
    ///
    /// Listeners and connected sockets share this mapping, so it lives in one
    /// place instead of being duplicated in every `check_io_events()`.
    #[inline]
    pub fn poll_events(&self) -> EPollEventType {
        let mut events = EPollEventType::empty();
        if self.is_recv_shutdown() {
            events |=
                EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM | EPollEventType::EPOLLRDHUP;
        }
        if self.is_both_shutdown() {
            events |= EPollEventType::EPOLLHUP;
        }
        events
    }

    /// Whether every bit in `other` is set.
    #[inline]
    pub fn contains(&self, other: ShutdownBit) -> bool {
        (self.bit & other.bit) == other.bit
    }

    pub fn is_recv_shutdown(&self) -> bool {
        self.bit & Self::RCV_SHUTDOWN != 0
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.bit & Self::SEND_SHUTDOWN != 0
    }

    pub fn is_both_shutdown(&self) -> bool {
        self.bit & Self::SHUTDOWN_MASK == Self::SHUTDOWN_MASK
    }

    pub fn is_empty(&self) -> bool {
        self.bit == 0
    }
}

impl TryFrom<usize> for ShutdownBit {
    type Error = system_error::SystemError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        // Linux/POSIX shutdown(2):
        //   0 = SHUT_RD, 1 = SHUT_WR, 2 = SHUT_RDWR
        match value {
            // SHUT_RD = 0, SHUT_WR = 1, SHUT_RDWR = 2
            0..=2 => Ok(ShutdownBit {
                bit: value as u8 + 1,
            }),
            _ => Err(Self::Error::EINVAL),
        }
    }
}
