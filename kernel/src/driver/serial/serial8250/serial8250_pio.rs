//! PIO的串口驱动

use core::{
    hint::spin_loop,
    sync::atomic::{AtomicBool, Ordering},
};

use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};

use crate::{
    arch::{driver::apic::ioapic::IoApic, io::PortIOArch, CurrentPortIOArch},
    driver::{
        base::device::{
            device_number::{DeviceNumber, Major},
            DeviceId,
        },
        serial::{AtomicBaudRate, BaudRate, DivisorFraction, UartPort},
        tty::{
            console::ConsoleSwitch,
            kthread::send_to_tty_refresh_thread,
            termios::WindowSize,
            tty_core::{TtyCore, TtyCoreData},
            tty_driver::{TtyDriver, TtyDriverManager, TtyOperation},
            virtual_terminal::{vc_manager, virtual_console::VirtualConsoleData, VirtConsole},
        },
        video::console::dummycon::dummy_console,
    },
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandleFlags, IrqHandler, IrqReturn},
        manage::irq_manager,
        IrqNumber,
    },
    libs::{rwlock::RwLock, spinlock::SpinLock},
};
use system_error::SystemError;

use super::{Serial8250ISADevices, Serial8250ISADriver, Serial8250Manager, Serial8250Port};

static mut PIO_PORTS: [Option<Serial8250PIOPort>; 8] =
    [None, None, None, None, None, None, None, None];

const SERIAL_8250_PIO_IRQ: IrqNumber = IrqNumber::new(IoApic::VECTOR_BASE as u32 + 4);

impl Serial8250Manager {
    #[allow(static_mut_refs)]
    pub(super) fn bind_pio_ports(
        &self,
        uart_driver: &Arc<Serial8250ISADriver>,
        devs: &Arc<Serial8250ISADevices>,
    ) {
        for port in unsafe { &PIO_PORTS }.iter().flatten() {
            port.set_device(Some(devs));
            self.uart_add_one_port(uart_driver, port).ok();
        }
    }
}

macro_rules! init_port {
    ($port_num:expr) => {
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
                crate::driver::serial::SERIAL_BAUDRATE,
            );
            if let Ok(port) = port {
                if port.init().is_ok() {
                    PIO_PORTS[$port_num - 1] = Some(port);
                    true
                } else {
                    false
                }
            } else {
                false
            }
        }
    };
}

/// 在内存管理初始化之前，初始化串口设备
pub(super) fn serial8250_pio_port_early_init() -> Result<(), SystemError> {
    for i in 1..=8 {
        init_port!(i);
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

        r.check_baudrate(&baudrate)?;

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
            CurrentPortIOArch::out8(port, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

            // Check if serial is faulty (i.e: not same byte as sent)
            if CurrentPortIOArch::in8(port) != 0xAE {
                self.initialized.store(false, Ordering::SeqCst);
                return Err(SystemError::ENODEV);
            }

            // If serial is not faulty set it in normal operation mode
            // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
            CurrentPortIOArch::out8(port + 4, 0x0b);

            CurrentPortIOArch::out8(port + 1, 0x01); // Enable interrupts
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
        self.serial_in(5) & 1 != 0
    }

    fn is_transmit_empty(&self) -> bool {
        self.serial_in(5) & 0x20 != 0
    }

    /// 发送字节
    ///
    /// ## 参数
    ///
    /// - `s`：待发送的字节
    fn send_bytes(&self, s: &[u8]) {
        while !self.is_transmit_empty() {
            spin_loop();
        }

        for c in s {
            self.serial_out(0, (*c).into());
        }
    }

    /// 读取一个字节，如果没有数据则返回None
    fn read_one_byte(&self) -> Option<u8> {
        if !self.serial_received() {
            return None;
        }
        return Some(self.serial_in(0) as u8);
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

            CurrentPortIOArch::out8(port, (divisor & 0xff) as u8); // Set divisor  (lo byte)
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
        let mut buf = [0; 8];
        let mut index = 0;

        // Read up to the size of the buffer
        while index < buf.len() {
            if let Some(c) = self.read_one_byte() {
                buf[index] = c;
                index += 1;
            } else {
                break; // No more bytes to read
            }
        }

        send_to_tty_refresh_thread(&buf[0..index]);
        Ok(())
    }

    fn iobase(&self) -> Option<usize> {
        Some(self.iobase as usize)
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

    #[allow(dead_code)]
    pub fn device(&self) -> Option<Arc<Serial8250ISADevices>> {
        if let Some(device) = self.device.as_ref() {
            return device.upgrade();
        }
        return None;
    }

    fn set_device(&mut self, device: Option<&Arc<Serial8250ISADevices>>) {
        self.device = device.map(Arc::downgrade);
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
pub fn send_to_default_serial8250_pio_port(s: &[u8]) {
    if let Some(port) = unsafe { PIO_PORTS[0].as_ref() } {
        port.send_bytes(s);
    }
}

#[derive(Debug)]
pub(super) struct Serial8250PIOTtyDriverInner;

impl Serial8250PIOTtyDriverInner {
    pub fn new() -> Self {
        Self
    }

    fn do_install(
        &self,
        driver: Arc<TtyDriver>,
        tty: Arc<TtyCore>,
        vc: Arc<VirtConsole>,
    ) -> Result<(), SystemError> {
        driver.standard_install(tty.clone())?;
        vc.port().setup_internal_tty(Arc::downgrade(&tty));
        tty.set_port(vc.port());
        vc.devfs_setup()?;

        Ok(())
    }
}

impl TtyOperation for Serial8250PIOTtyDriverInner {
    fn open(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Ok(())
    }

    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        let index = tty.index();
        if tty.index() >= unsafe { PIO_PORTS.len() } {
            return Err(SystemError::ENODEV);
        }
        let pio_port = unsafe { PIO_PORTS[index].as_ref() }.ok_or(SystemError::ENODEV)?;
        pio_port.send_bytes(&buf[..nr]);

        Ok(nr)
    }

    fn flush_chars(&self, _tty: &TtyCoreData) {}

    fn put_char(&self, tty: &TtyCoreData, ch: u8) -> Result<(), SystemError> {
        self.write(tty, &[ch], 1).map(|_| ())
    }

    fn ioctl(&self, _tty: Arc<TtyCore>, _cmd: u32, _arg: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOIOCTLCMD)
    }

    fn close(&self, _tty: Arc<TtyCore>) -> Result<(), SystemError> {
        Ok(())
    }

    fn resize(&self, tty: Arc<TtyCore>, winsize: WindowSize) -> Result<(), SystemError> {
        *tty.core().window_size_write() = winsize;
        Ok(())
    }

    fn install(&self, driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        if tty.core().index() >= unsafe { PIO_PORTS.len() } {
            return Err(SystemError::ENODEV);
        }

        *tty.core().window_size_write() = WindowSize::DEFAULT;
        let vc_data = Arc::new(SpinLock::new(VirtualConsoleData::new(usize::MAX)));
        let mut vc_data_guard = vc_data.lock_irqsave();
        vc_data_guard.set_driver_funcs(Arc::downgrade(&dummy_console()) as Weak<dyn ConsoleSwitch>);
        vc_data_guard.init(
            Some(tty.core().window_size().row.into()),
            Some(tty.core().window_size().col.into()),
            true,
        );
        drop(vc_data_guard);
        let vc = VirtConsole::new(Some(vc_data));
        let vc_index = vc_manager().alloc(vc.clone()).ok_or(SystemError::EBUSY)?;
        self.do_install(driver, tty, vc.clone()).inspect_err(|_| {
            vc_manager().free(vc_index);
        })?;

        Ok(())
    }
}

pub(super) fn serial_8250_pio_register_tty_devices() -> Result<(), SystemError> {
    let (_, driver) = TtyDriverManager::lookup_tty_driver(DeviceNumber::new(
        Major::TTY_MAJOR,
        Serial8250Manager::TTY_SERIAL_MINOR_START,
    ))
    .ok_or(SystemError::ENODEV)?;

    for (i, port) in unsafe { PIO_PORTS.iter() }.enumerate() {
        if let Some(port) = port {
            let core = driver.init_tty_device(Some(i)).inspect_err(|_| {
                log::error!(
                    "failed to init tty device for serial 8250 pio port {}, port iobase: {:?}",
                    i,
                    port.iobase
                );
            })?;
            core.resize( core.clone(), WindowSize::DEFAULT)
                .inspect_err(|_| {
                    log::error!(
                        "failed to resize tty device for serial 8250 pio port {}, port iobase: {:?}",
                        i,
                        port.iobase
                    );
                })?;
        }
    }

    irq_manager()
        .request_irq(
            SERIAL_8250_PIO_IRQ,
            "serial8250_pio".to_string(),
            &Serial8250IrqHandler,
            IrqHandleFlags::IRQF_SHARED | IrqHandleFlags::IRQF_TRIGGER_RISING,
            Some(DeviceId::new(Some("serial8250_pio"), None).unwrap()),
        )
        .inspect_err(|e| {
            log::error!("failed to request irq for serial 8250 pio: {:?}", e);
        })?;

    Ok(())
}

#[derive(Debug)]
struct Serial8250IrqHandler;

impl IrqHandler for Serial8250IrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        for port in unsafe { PIO_PORTS.iter() }.flatten() {
            port.handle_irq()?;
        }

        Ok(IrqReturn::Handled)
    }
}
