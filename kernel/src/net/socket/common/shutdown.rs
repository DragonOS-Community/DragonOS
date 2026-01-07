/// Shutdown bit for socket operations.
pub struct ShutdownBit {
    bit: u8,
}

impl ShutdownBit {
    const RCV_SHUTDOWN: u8 = 0x01;
    const SEND_SHUTDOWN: u8 = 0x02;
    const SHUTDOWN_MASK: u8 = 0x03;

    // 兼容 Linux/POSIX shutdown(2) 语义的公开常量（面向调用点）。
    pub const SHUT_RD: ShutdownBit = ShutdownBit {
        bit: Self::RCV_SHUTDOWN,
    };
    pub const SHUT_WR: ShutdownBit = ShutdownBit {
        bit: Self::SEND_SHUTDOWN,
    };
    pub const SHUT_RDWR: ShutdownBit = ShutdownBit {
        bit: Self::RCV_SHUTDOWN | Self::SEND_SHUTDOWN,
    };

    /// 返回内部 bit 掩码。
    #[inline]
    pub fn bits(&self) -> u8 {
        self.bit
    }

    /// 从原始整数生成 ShutdownBit（截断非法位）。
    ///
    /// 注意：这里的 raw 是内部状态位（RCV/SEND），而不是 shutdown(2) 的 how 参数。
    #[inline]
    pub fn from_bits_truncate(raw: usize) -> ShutdownBit {
        ShutdownBit {
            bit: (raw as u8) & Self::SHUTDOWN_MASK,
        }
    }

    /// 判断是否包含给定 shutdown 位。
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
