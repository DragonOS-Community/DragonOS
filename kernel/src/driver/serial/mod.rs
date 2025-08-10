use core::{fmt::Debug, sync::atomic::AtomicU32};

use alloc::sync::Arc;
use system_error::SystemError;

use crate::mm::VirtAddr;

use self::serial8250::serial8250_manager;

use super::tty::{
    termios::{ControlMode, InputMode, LocalMode, OutputMode, Termios, INIT_CONTORL_CHARACTERS},
    tty_ldisc::LineDisciplineType,
};

pub mod serial8250;

#[allow(dead_code)]
pub trait UartDriver: Debug + Send + Sync {
    /// 获取最大的设备数量
    fn max_devs_num(&self) -> i32;

    // todo: 获取指向console的指针（在我们系统里面，将来可能是改进后的Textui Window）
}

pub const SERIAL_BAUDRATE: BaudRate = BaudRate::new(115200);

/// 串口端口应当实现的trait
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serial_core.h#428
#[allow(dead_code)]
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

lazy_static! {
    pub static ref TTY_SERIAL_DEFAULT_TERMIOS: Termios = {
        Termios {
            input_mode: InputMode::ICRNL | InputMode::IXON | InputMode::IUTF8,
            output_mode: OutputMode::OPOST | OutputMode::ONLCR,
            control_mode: ControlMode::B115200
                | ControlMode::CREAD
                | ControlMode::HUPCL
                | ControlMode::CS8,
            local_mode: LocalMode::ISIG
                | LocalMode::ICANON
                | LocalMode::ECHO
                | LocalMode::ECHOE
                | LocalMode::ECHOK
                | LocalMode::ECHOCTL
                | LocalMode::ECHOKE
                | LocalMode::IEXTEN,
            control_characters: INIT_CONTORL_CHARACTERS,
            line: LineDisciplineType::NTty,
            input_speed: SERIAL_BAUDRATE.data(),
            output_speed: SERIAL_BAUDRATE.data(),
        }
    };
}

#[inline(always)]
#[allow(dead_code)]
pub(super) fn uart_manager() -> &'static UartManager {
    &UartManager
}
#[derive(Debug)]
pub(super) struct UartManager;

impl UartManager {
    pub const NR_TTY_SERIAL_MAX: u32 = 128;
    /// todo: 把uart设备注册到tty层
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/tty/serial/serial_core.c?fi=uart_register_driver#2720
    #[allow(dead_code)]
    pub fn register_driver(&self, _driver: &Arc<dyn UartDriver>) -> Result<(), SystemError> {
        return Ok(());
    }
}

pub fn serial_early_init() -> Result<(), SystemError> {
    serial8250_manager().early_init()?;
    return Ok(());
}

pub(super) fn serial_init() -> Result<(), SystemError> {
    serial8250_manager().init()?;
    return Ok(());
}
