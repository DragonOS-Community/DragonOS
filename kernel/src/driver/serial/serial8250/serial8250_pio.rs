//! PIO的串口驱动

use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::sync::{Arc, Weak};

use crate::{
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::serial::{AtomicBaudRate, BaudRate, DivisorFraction, UartPort},
    libs::rwlock::RwLock,
};
use system_error::SystemError;

use super::{Serial8250ISADevices, Serial8250ISADriver, Serial8250Manager, Serial8250Port};

static mut PIO_PORTS: [Option<Serial8250PIOPort>; 8] =
    [None, None, None, None, None, None, None, None];

impl Serial8250Manager {
    pub(super) fn bind_pio_ports(
        &self,
        uart_driver: &Arc<Serial8250ISADriver>,
        devs: &Arc<Serial8250ISADevices>,
    ) {
        for i in 0..8 {
            if let Some(port) = unsafe { PIO_PORTS[i].as_ref() } {
                port.set_device(Some(devs));
                self.uart_add_one_port(uart_driver, port).ok();
            }
        }
    }
}

macro_rules! init_port {
    ($port_num:expr, $baudrate:expr) => {
        unsafe {
            let port = Serial8250PIOPort::new(
                match $port_num {
                    1 => Serial8250PortBase::COM1,
                    2 => Serial8250PortBase::COM2,
                    3 => Serial8250PortBase::COM3,
                    4 => Serial8250PortBase::COM4,
                    5 => Serial8250PortBase::COM5,
                    6 => Serial8250PortBase::COM6,
                    7 => Serial8250PortBase::COM7,
                    8 => Serial8250PortBase::COM8,
                    _ => panic!("invalid port number"),
                },
                BaudRate::new($baudrate),
            );
            if let Ok(port) = port {
                if port.init().is_ok() {
                    PIO_PORTS[$port_num - 1] = Some(port);
                }
            }
        }
    };
}

/// 在内存管理初始化之前，初始化串口设备
pub(super) fn serial8250_pio_port_early_init() -> Result<(), SystemError> {
    for i in 1..=8 {
        init_port!(i, 115200);
    }
    return Ok(());
}

#[derive(Debug)]
pub struct Serial8250PIOPort {
    iobase: Serial8250PortBase,
    baudrate: AtomicBaudRate,
    initialized: AtomicBool,
    inner: RwLock<Serial8250PIOPortInner>,
}

impl Serial8250PIOPort {
    const SERIAL8250PIO_MAX_BAUD_RATE: BaudRate = BaudRate::new(115200);
    pub fn new(iobase: Serial8250PortBase, baudrate: BaudRate) -> Result<Self, SystemError> {
        let r = Self {
            iobase,
            baudrate: AtomicBaudRate::new(baudrate),
            initialized: AtomicBool::new(false),
            inner: RwLock::new(Serial8250PIOPortInner::new()),
        };

        if let Err(e) = r.check_baudrate(&baudrate) {
            return Err(e);
        }

        return Ok(r);
    }

    pub fn init(&self) -> Result<(), SystemError> {
        let r = self
            .initialized
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst);
        if r.is_err() {
            // 已经初始化
            return Ok(());
        }

        let port = self.iobase as u16;

        unsafe {
            CurrentPortIOArch::out8(port + 1, 0x00); // Disable all interrupts
            self.set_divisor(self.baudrate.load(Ordering::SeqCst))
                .unwrap(); // Set baud rate

            CurrentPortIOArch::out8(port + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
            CurrentPortIOArch::out8(port + 4, 0x08); // IRQs enabled, RTS/DSR clear (现代计算机上一般都不需要hardware flow control，因此不需要置位RTS/DSR)
            CurrentPortIOArch::out8(port + 4, 0x1E); // Set in loopback mode, test the serial chip
            CurrentPortIOArch::out8(port + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

            // Check if serial is faulty (i.e: not same byte as sent)
            if CurrentPortIOArch::in8(port + 0) != 0xAE {
                self.initialized.store(false, Ordering::SeqCst);
                return Err(SystemError::ENODEV);
            }

            // If serial is not faulty set it in normal operation mode
            // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
            CurrentPortIOArch::out8(port + 4, 0x08);
        }

        return Ok(());
        /*
                Notice that the initialization code above writes to [PORT + 1]
            twice with different values. This is once to write to the Divisor
            register along with [PORT + 0] and once to write to the Interrupt
            register as detailed in the previous section.
                The second write to the Line Control register [PORT + 3]
            clears the DLAB again as well as setting various other bits.
        */
    }

    const fn check_baudrate(&self, baudrate: &BaudRate) -> Result<(), SystemError> {
        // 错误的比特率
        if baudrate.data() > Self::SERIAL8250PIO_MAX_BAUD_RATE.data()
            || Self::SERIAL8250PIO_MAX_BAUD_RATE.data() % baudrate.data() != 0
        {
            return Err(SystemError::EINVAL);
        }

        return Ok(());
    }

    #[allow(dead_code)]
    fn serial_received(&self) -> bool {
        if self.serial_in(5) & 1 != 0 {
            true
        } else {
            false
        }
    }

    fn is_transmit_empty(&self) -> bool {
        if self.serial_in(5) & 0x20 != 0 {
            true
        } else {
            false
        }
    }

    /// 发送字节
    ///
    /// ## 参数
    ///
    /// - `s`：待发送的字节
    fn send_bytes(&self, s: &[u8]) {
        while self.is_transmit_empty() == false {
            spin_loop();
        }

        for c in s {
            self.serial_out(0, (*c).into());
        }
    }

    /// 读取一个字节
    #[allow(dead_code)]
    fn read_one_byte(&self) -> u8 {
        while self.serial_received() == false {
            spin_loop();
        }
        return self.serial_in(0) as u8;
    }
}

impl Serial8250Port for Serial8250PIOPort {
    fn device(&self) -> Option<Arc<Serial8250ISADevices>> {
        self.inner.read().device()
    }

    fn set_device(&self, device: Option<&Arc<Serial8250ISADevices>>) {
        self.inner.write().set_device(device);
    }
}

impl UartPort for Serial8250PIOPort {
    fn serial_in(&self, offset: u32) -> u32 {
        unsafe { CurrentPortIOArch::in8(self.iobase as u16 + offset as u16).into() }
    }

    fn serial_out(&self, offset: u32, value: u32) {
        // warning: pio的串口只能写入8位，因此这里丢弃高24位
        unsafe { CurrentPortIOArch::out8(self.iobase as u16 + offset as u16, value as u8) }
    }

    fn divisor(&self, baud: BaudRate) -> (u32, DivisorFraction) {
        let divisor = Self::SERIAL8250PIO_MAX_BAUD_RATE.data() / baud.data();
        return (divisor, DivisorFraction::new(0));
    }

    fn set_divisor(&self, baud: BaudRate) -> Result<(), SystemError> {
        self.check_baudrate(&baud)?;

        let port = self.iobase as u16;
        unsafe {
            CurrentPortIOArch::out8(port + 3, 0x80); // Enable DLAB (set baud rate divisor)

            let divisor = self.divisor(baud).0;

            CurrentPortIOArch::out8(port + 0, (divisor & 0xff) as u8); // Set divisor  (lo byte)
            CurrentPortIOArch::out8(port + 1, ((divisor >> 8) & 0xff) as u8); // (hi byte)
            CurrentPortIOArch::out8(port + 3, 0x03); // 8 bits, no parity, one stop bit
        }

        self.baudrate.store(baud, Ordering::SeqCst);

        return Ok(());
    }

    fn startup(&self) -> Result<(), SystemError> {
        todo!("serial8250_pio::startup")
    }

    fn shutdown(&self) {
        todo!("serial8250_pio::shutdown")
    }

    fn baud_rate(&self) -> Option<BaudRate> {
        Some(self.baudrate.load(Ordering::SeqCst))
    }

    fn handle_irq(&self) -> Result<(), SystemError> {
        todo!("serial8250_pio::handle_irq")
    }
}

#[derive(Debug)]
struct Serial8250PIOPortInner {
    /// 当前端口绑定的设备
    ///
    /// ps: 存储weak以避免循环引用
    device: Option<Weak<Serial8250ISADevices>>,
}

impl Serial8250PIOPortInner {
    pub const fn new() -> Self {
        Self { device: None }
    }

    pub fn device(&self) -> Option<Arc<Serial8250ISADevices>> {
        if let Some(device) = self.device.as_ref() {
            return device.upgrade();
        }
        return None;
    }

    fn set_device(&mut self, device: Option<&Arc<Serial8250ISADevices>>) {
        self.device = device.map(|d| Arc::downgrade(d));
    }
}

#[allow(dead_code)]
#[repr(u16)]
#[derive(Clone, Debug, Copy)]
pub enum Serial8250PortBase {
    COM1 = 0x3f8,
    COM2 = 0x2f8,
    COM3 = 0x3e8,
    COM4 = 0x2e8,
    COM5 = 0x5f8,
    COM6 = 0x4f8,
    COM7 = 0x5e8,
    COM8 = 0x4e8,
}

/// 临时函数，用于向COM1发送数据
pub fn send_to_serial8250_pio_com1(s: &[u8]) {
    if let Some(port) = unsafe { PIO_PORTS[0].as_ref() } {
        port.send_bytes(s);
    }
}
