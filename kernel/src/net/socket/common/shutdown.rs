/// Shutdown bit for socket operations.
pub struct ShutdownBit {
    bit: u8,
}

impl ShutdownBit {
    const RCV_SHUTDOWN: u8 = 0x01;
    const SEND_SHUTDOWN: u8 = 0x02;
    const SHUTDOWN_MASK: u8 = 0x03;
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
        match value {
            0..2 => Ok(ShutdownBit {
                bit: value as u8 + 1,
            }),
            _ => Err(Self::Error::EINVAL),
        }
    }
}
