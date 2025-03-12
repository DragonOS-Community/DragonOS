// TODO: 其他模块需要实现shutdown的具体逻辑
#![allow(dead_code)]
use core::sync::atomic::AtomicU8;

use system_error::SystemError;

bitflags! {
    /// @brief 用于指定socket的关闭类型
    /// 参考：https://code.dragonos.org.cn/xref/linux-6.1.9/include/net/sock.h?fi=SHUTDOWN_MASK#1573
    pub struct ShutdownBit: u8 {
        const SHUT_RD = 0;
        const SHUT_WR = 1;
        const SHUT_RDWR = 2;
    }
}

const RCV_SHUTDOWN: u8 = 0x01;
const SEND_SHUTDOWN: u8 = 0x02;
const SHUTDOWN_MASK: u8 = 0x03;

#[derive(Debug, Default)]
pub struct Shutdown {
    bit: AtomicU8,
}

impl From<ShutdownBit> for Shutdown {
    fn from(shutdown_bit: ShutdownBit) -> Self {
        match shutdown_bit {
            ShutdownBit::SHUT_RD => Shutdown {
                bit: AtomicU8::new(RCV_SHUTDOWN),
            },
            ShutdownBit::SHUT_WR => Shutdown {
                bit: AtomicU8::new(SEND_SHUTDOWN),
            },
            ShutdownBit::SHUT_RDWR => Shutdown {
                bit: AtomicU8::new(SHUTDOWN_MASK),
            },
            _ => Shutdown::default(),
        }
    }
}

impl Shutdown {
    pub fn new() -> Self {
        Self {
            bit: AtomicU8::new(0),
        }
    }

    pub fn recv_shutdown(&self) {
        self.bit
            .fetch_or(RCV_SHUTDOWN, core::sync::atomic::Ordering::SeqCst);
    }

    pub fn send_shutdown(&self) {
        self.bit
            .fetch_or(SEND_SHUTDOWN, core::sync::atomic::Ordering::SeqCst);
    }

    pub fn is_recv_shutdown(&self) -> bool {
        self.bit.load(core::sync::atomic::Ordering::SeqCst) & RCV_SHUTDOWN != 0
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.bit.load(core::sync::atomic::Ordering::SeqCst) & SEND_SHUTDOWN != 0
    }

    pub fn is_both_shutdown(&self) -> bool {
        self.bit.load(core::sync::atomic::Ordering::SeqCst) & SHUTDOWN_MASK == SHUTDOWN_MASK
    }

    pub fn is_empty(&self) -> bool {
        self.bit.load(core::sync::atomic::Ordering::SeqCst) == 0
    }

    pub fn from_how(how: usize) -> Self {
        Self::from(ShutdownBit::from_bits_truncate(how as u8))
    }

    pub fn get(&self) -> ShutdownTemp {
        ShutdownTemp {
            bit: self.bit.load(core::sync::atomic::Ordering::SeqCst),
        }
    }
}

pub struct ShutdownTemp {
    bit: u8,
}

impl ShutdownTemp {
    pub fn is_recv_shutdown(&self) -> bool {
        self.bit & RCV_SHUTDOWN != 0
    }

    pub fn is_send_shutdown(&self) -> bool {
        self.bit & SEND_SHUTDOWN != 0
    }

    pub fn is_both_shutdown(&self) -> bool {
        self.bit & SHUTDOWN_MASK == SHUTDOWN_MASK
    }

    pub fn is_empty(&self) -> bool {
        self.bit == 0
    }

    pub fn bits(&self) -> ShutdownBit {
        ShutdownBit { bits: self.bit }
    }
}

impl From<ShutdownBit> for ShutdownTemp {
    fn from(shutdown_bit: ShutdownBit) -> Self {
        match shutdown_bit {
            ShutdownBit::SHUT_RD => Self { bit: RCV_SHUTDOWN },
            ShutdownBit::SHUT_WR => Self { bit: SEND_SHUTDOWN },
            ShutdownBit::SHUT_RDWR => Self { bit: SHUTDOWN_MASK },
            _ => Self { bit: 0 },
        }
    }
}

impl TryFrom<usize> for ShutdownTemp {
    type Error = SystemError;

    fn try_from(value: usize) -> Result<Self, Self::Error> {
        match value {
            0..2 => Ok(ShutdownTemp {
                bit: value as u8 + 1,
            }),
            _ => Err(SystemError::EINVAL),
        }
    }
}
