use core::{fmt::Debug, sync::atomic::AtomicU32};

use crate::{driver::base::device::DeviceNumber, mm::VirtAddr, syscall::SystemError};

use super::tty_driver::TtyDriver;

pub mod serial8250;

pub trait UartDriver: Debug + Send + Sync + TtyDriver {
    fn device_number(&self) -> DeviceNumber;

    /// 获取设备数量
    fn devs_num(&self) -> i32;

    // todo: 获取指向console的指针（在我们系统里面，将来可能是改进后的Textui Window）
}

/// 串口端口应当实现的trait
///
/// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/include/linux/serial_core.h#428
pub trait UartPort {
    fn iobase(&self) -> Option<usize> {
        None
    }
    fn membase(&self) -> Option<VirtAddr> {
        None
    }
    fn serial_in(&self, offset: u32) -> u32;
    fn serial_out(&self, offset: u32, value: u32);
    fn divisor(&self, baud: BaudRate) -> (u32, DivisorFraction);
    fn set_divisor(&self, baud: BaudRate) -> Result<(), SystemError>;
    fn baud_rate(&self) -> Option<BaudRate>;
    fn startup(&self) -> Result<(), SystemError>;
    fn shutdown(&self);
    fn handle_irq(&self) -> Result<(), SystemError>;
}

int_like!(BaudRate, AtomicBaudRate, u32, AtomicU32);
int_like!(DivisorFraction, u32);
