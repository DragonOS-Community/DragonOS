use super::super::base::device::Device;
use crate::{
    driver::base::{
        char::CharDevice,
    device::{driver::Driver, DeviceState, DeviceType, IdTable, KObject},
        platform::{
            self, platform_device::PlatformDevice, platform_driver::PlatformDriver, CompatibleTable,
        },
    },
    filesystem::{
        sysfs::bus::{bus_device_register, bus_driver_register},
        vfs::IndexNode,
    },
    include::bindings::bindings::{io_in8, io_out8},
    libs::spinlock::SpinLock,
    syscall::SystemError,
};
use alloc::sync::Arc;
use core::{char, intrinsics::offset, str};

const UART_SUCCESS: i32 = 0;
const E_UART_BITS_RATE_ERROR: i32 = 1;
const E_UART_SERIAL_FAULT: i32 = 2;
const UART_MAX_BITS_RATE: u32 = 115200;

lazy_static! {
    // 串口设备
    pub static ref UART_DEV: Arc<LockUart> = Arc::new(LockUart::default());
    // 串口驱动
    pub static ref UART_DRV: Arc<LockUartDriver> = Arc::new(LockUartDriver::default());
}

// @brief 串口端口
#[allow(dead_code)]
#[repr(u16)]
#[derive(Clone, Debug)]
pub enum UartPort {
    COM1 = 0x3f8,
    COM2 = 0x2f8,
    COM3 = 0x3e8,
    COM4 = 0x2e8,
    COM5 = 0x5f8,
    COM6 = 0x4f8,
    COM7 = 0x5e8,
    COM8 = 0x4e8,
}

impl UartPort {
    ///@brief 将u16转换为UartPort枚举类型
    ///@param val 要转换的u16类型
    ///@return 输入的端口地址正确，返回UartPort类型，错误，返回错误信息
    #[allow(dead_code)]
    pub fn from_u16(val: u16) -> Result<Self, &'static str> {
        match val {
            0x3f8 => Ok(Self::COM1),
            0x2f8 => Ok(Self::COM2),
            0x3e8 => Ok(Self::COM3),
            0x2e8 => Ok(Self::COM4),
            0x5f8 => Ok(Self::COM5),
            0x4f8 => Ok(Self::COM6),
            0x5e8 => Ok(Self::COM7),
            0x4e8 => Ok(Self::COM8),
            _ => Err("port error!"),
        }
    }

    ///@brief 将UartPort枚举类型转换为u16类型
    ///@param self 要转换的UartPort
    ///@return 转换的u16值
    #[allow(dead_code)]
    pub fn to_u16(self: &Self) -> u16 {
        match self {
            Self::COM1 => 0x3f8,
            Self::COM2 => 0x2f8,
            Self::COM3 => 0x3e8,
            Self::COM4 => 0x2e8,
            Self::COM5 => 0x5f8,
            Self::COM6 => 0x4f8,
            Self::COM7 => 0x5e8,
            Self::COM8 => 0x4e8,
        }
    }
}

// @brief 串口寄存器
#[allow(dead_code)]
#[repr(C)]
#[derive(Debug, Copy, Clone)]
struct UartRegister {
    reg_data: u8,
    reg_interrupt_enable: u8,
    reg_ii_fifo: u8, // 	Interrupt Identification and FIFO control registers
    reg_line_config: u8,
    reg_modem_config: u8,
    reg_line_status: u8,
    reg_modem_statue: u8,
    reg_scartch: u8,
}

// @brief 串口设备结构体
#[derive(Debug)]
pub struct Uart {
    state: DeviceState, // 设备状态
    sys_info: Option<Arc<dyn IndexNode>>,
    driver: Option<Arc<dyn PlatformDriver>>,
}

impl Default for Uart {
    fn default() -> Self {
        Self {
            state: DeviceState::NotInitialized,
            sys_info: None,
            driver: None,
        }
    }
}

// @brief 串口设备结构体(加锁)
#[derive(Debug)]
pub struct LockUart(SpinLock<Uart>);

impl Default for LockUart {
    fn default() -> Self {
        Self(SpinLock::new(Uart::default()))
    }
}

impl KObject for LockUart {}

impl PlatformDevice for LockUart {
    fn compatible_table(&self) -> platform::CompatibleTable {
        platform::CompatibleTable::new(vec!["uart"])
    }

    fn is_initialized(&self) -> bool {
        let state = self.0.lock().state;
        match state {
            DeviceState::Initialized => true,
            _ => false,
        }
    }

    fn set_state(&self, set_state: DeviceState) {
        let state = &mut self.0.lock().state;
        *state = set_state;
    }

    fn set_driver(&self, driver: Option<Arc<dyn PlatformDriver>>) {
        self.0.lock().driver = driver;
    }
}

impl Device for LockUart {
    fn id_table(&self) -> IdTable {
        IdTable::new("uart", 0)
    }

    fn set_sys_info(&self, sys_info: Option<Arc<dyn IndexNode>>) {
        self.0.lock().sys_info = sys_info;
    }

    fn sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        self.0.lock().sys_info.clone()
    }

    fn dev_type(&self) -> DeviceType {
        DeviceType::Serial
    }

    fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
        self
    }
}

// @brief 串口驱动结构体
#[repr(C)]
#[derive(Debug)]
pub struct UartDriver {
    port: UartPort,
    baud_rate: u32,
    sys_info: Option<Arc<dyn IndexNode>>,
}

impl Default for UartDriver {
    fn default() -> Self {
        Self {
            port: UartPort::COM1,
            baud_rate: 115200,
            sys_info: None,
        }
    }
}

// @brief 串口驱动结构体(加锁)
#[derive(Debug)]
pub struct LockUartDriver(SpinLock<UartDriver>);

impl Default for LockUartDriver {
    fn default() -> Self {
        Self(SpinLock::new(UartDriver::default()))
    }
}

impl KObject for LockUartDriver {}

impl Driver for LockUartDriver {
    fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
        self
    }

    fn id_table(&self) -> IdTable {
        return IdTable::new("uart_driver", 0);
    }

    fn set_sys_info(&self, sys_info: Option<Arc<dyn IndexNode>>) {
        self.0.lock().sys_info = sys_info;
    }

    fn sys_info(&self) -> Option<Arc<dyn IndexNode>> {
        return self.0.lock().sys_info.clone();
    }
}

impl CharDevice for LockUartDriver {
    fn open(&self, _file: Arc<dyn IndexNode>) -> Result<(), crate::syscall::SystemError> {
        return Ok(());
    }

    fn close(&self, _file: Arc<dyn IndexNode>) -> Result<(), crate::syscall::SystemError> {
        return Ok(());
    }
}

impl LockUartDriver {
    /// @brief 创建串口驱动
    /// @param port 端口号
    ///        baud_rate 波特率
    ///        sys_info: sys文件系统inode
    /// @return  
    #[allow(dead_code)]
    pub fn new(port: UartPort, baud_rate: u32, sys_info: Option<Arc<dyn IndexNode>>) -> Self {
        Self(SpinLock::new(UartDriver::new(port, baud_rate, sys_info)))
    }
}

impl PlatformDriver for LockUartDriver {
    fn probe(
        &self,
        _device: Arc<dyn PlatformDevice>,
    ) -> Result<(), crate::driver::base::device::driver::DriverError> {
        return Ok(());
    }

    fn compatible_table(&self) -> platform::CompatibleTable {
        return CompatibleTable::new(vec!["uart"]);
    }
}

impl UartDriver {
    /// @brief 创建串口驱动
    /// @param port 端口号
    ///        baud_rate 波特率
    ///        sys_info: sys文件系统inode
    /// @return 返回串口驱动
    #[allow(dead_code)]
    pub fn new(port: UartPort, baud_rate: u32, sys_info: Option<Arc<dyn IndexNode>>) -> Self {
        Self {
            port,
            baud_rate,
            sys_info,
        }
    }

    /// @brief 串口初始化
    /// @param uart_port 端口号
    /// @param baud_rate 波特率
    /// @return 初始化成功，返回0,失败，返回错误信息
    #[allow(dead_code)]
    pub fn uart_init(uart_port: &UartPort, baud_rate: u32) -> Result<i32, &'static str> {
        let message: &'static str = "uart init.";
        let port = uart_port.to_u16();
        // 错误的比特率
        if baud_rate > UART_MAX_BITS_RATE || UART_MAX_BITS_RATE % baud_rate != 0 {
            return Err("uart init error.");
        }

        unsafe {
            io_out8(port + 1, 0x00); // Disable all interrupts
            io_out8(port + 3, 0x80); // Enable DLAB (set baud rate divisor)

            let divisor = UART_MAX_BITS_RATE / baud_rate;

            io_out8(port + 0, (divisor & 0xff) as u8); // Set divisor  (lo byte)
            io_out8(port + 1, ((divisor >> 8) & 0xff) as u8); //                  (hi byte)
            io_out8(port + 3, 0x03); // 8 bits, no parity, one stop bit
            io_out8(port + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
            io_out8(port + 4, 0x08); // IRQs enabled, RTS/DSR clear (现代计算机上一般都不需要hardware flow control，因此不需要置位RTS/DSR)
            io_out8(port + 4, 0x1E); // Set in loopback mode, test the serial chip
            io_out8(port + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

            // Check if serial is faulty (i.e: not same byte as sent)
            if io_in8(port + 0) != 0xAE {
                return Err("uart faulty");
            }

            // If serial is not faulty set it in normal operation mode
            // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
            io_out8(port + 4, 0x08);
        }
        UartDriver::uart_send(uart_port, message);
        Ok(0)
        /*
                Notice that the initialization code above writes to [PORT + 1]
            twice with different values. This is once to write to the Divisor
            register along with [PORT + 0] and once to write to the Interrupt
            register as detailed in the previous section.
                The second write to the Line Control register [PORT + 3]
            clears the DLAB again as well as setting various other bits.
        */
    }

    fn serial_received(offset: u16) -> bool {
        if unsafe { io_in8(offset + 5) } & 1 != 0 {
            true
        } else {
            false
        }
    }

    fn is_transmit_empty(offset: u16) -> bool {
        if unsafe { io_in8(offset + 5) } & 0x20 != 0 {
            true
        } else {
            false
        }
    }

    /// @brief 串口发送
    /// @param uart_port 端口号
    /// @param str 发送字符切片
    /// @return None
    #[allow(dead_code)]
    fn uart_send(uart_port: &UartPort, s: &str) {
        let port = uart_port.to_u16();
        while UartDriver::is_transmit_empty(port) == false {
            for c in s.bytes() {
                unsafe {
                    io_out8(port, c);
                }
            }
        } //TODO:pause
    }

    /// @brief 串口接收一个字节
    /// @param uart_port 端口号
    /// @return 接收的字节
    #[allow(dead_code)]
    fn uart_read_byte(uart_port: &UartPort) -> char {
        let port = uart_port.to_u16();
        while UartDriver::serial_received(port) == false {} //TODO:pause
        unsafe { io_in8(port) as char }
    }

    #[allow(dead_code)]
    fn port() -> u16 {
        UartPort::COM1.to_u16()
    }
}

///@brief 发送数据
///@param port 端口号
///@param c 要发送的数据
#[no_mangle]
pub extern "C" fn c_uart_send(port: u16, c: u8) {
    while UartDriver::is_transmit_empty(port) == false {} //TODO:pause
    unsafe {
        io_out8(port, c);
    }
}

///@brief 从uart接收数据
///@param port 端口号
///@return u8 接收到的数据
#[no_mangle]
pub extern "C" fn c_uart_read(port: u16) -> u8 {
    while UartDriver::serial_received(port) == false {} //TODO:pause
    unsafe { io_in8(port) }
}

///@brief 通过串口发送整个字符串
///@param port 串口端口
///@param str 字符串S
#[no_mangle]
pub extern "C" fn c_uart_send_str(port: u16, s: *const u8) {
    unsafe {
        let mut i = 0isize;
        while *offset(s, i) != '\0' as u8 {
            c_uart_send(port, *offset(s, i));
            i = i + 1;
        }
    }
}

/// @brief 串口初始化
/// @param u16 端口号
/// @param baud_rate 波特率
/// @return 初始化成功，返回0,失败，返回错误码
#[no_mangle]
pub extern "C" fn c_uart_init(port: u16, baud_rate: u32) -> i32 {
    let message: &'static str = "uart init\n";
    // 错误的比特率
    if baud_rate > UART_MAX_BITS_RATE || UART_MAX_BITS_RATE % baud_rate != 0 {
        return -E_UART_BITS_RATE_ERROR;
    }

    unsafe {
        io_out8(port + 1, 0x00); // Disable all interrupts
        io_out8(port + 3, 0x80); // Enable DLAB (set baud rate divisor)

        let divisor = UART_MAX_BITS_RATE / baud_rate;

        io_out8(port + 0, (divisor & 0xff) as u8); // Set divisor  (lo byte)
        io_out8(port + 1, ((divisor >> 8) & 0xff) as u8); //                  CompatibleTable(hi byte)
        io_out8(port + 3, 0x03); // 8 bits, no parity, one stop bit
        io_out8(port + 2, 0xC7); // Enable FIFO, clear them, with 14-byte threshold
        io_out8(port + 4, 0x08); // IRQs enabled, RTS/DSR clear (现代计算机上一般都不需要hardware flow control，因此不需要置位RTS/DSR)
        io_out8(port + 4, 0x1E); // Set in loopback mode, test the serial chip
        io_out8(port + 0, 0xAE); // Test serial chip (send byte 0xAE and check if serial returns same byte)

        // Check if serial is faulty (i.e: not same byte as sent)
        if io_in8(port + 0) != 0xAE {
            return -E_UART_SERIAL_FAULT;
        }

        // If serial is not faulty set it in normal operation mode
        // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
        io_out8(port + 4, 0x08);
        let bytes = message.as_bytes();
        for c in bytes {
            c_uart_send(port, *c);
        }
    }
    return UART_SUCCESS;
    /*
            Notice that the initialization code above writes to [PORT + 1]
        twice with different values. This is once to write to the Divisor
        register along with [PORT + 0] and once to write to the Interrupt
        register as detailed in the previous section.
            The second write to the Line Control register [PORT + 3]
        clears the DLAB again as well as setting various other bits.
    */
}

/// @brief 串口初始化，注册串口
/// @param none
/// @return 初始化成功，返回(),失败，返回错误码
pub fn uart_init() -> Result<(), SystemError> {
    let device_inode = bus_device_register("platform:0", &UART_DEV.id_table().to_name())
        .expect("uart device register error");
    UART_DEV.set_sys_info(Some(device_inode));
    let driver_inode = bus_driver_register("platform:0", &UART_DRV.id_table().to_name())
        .expect("uart driver register error");
    UART_DRV.set_sys_info(Some(driver_inode));
    UART_DEV.set_driver(Some(UART_DRV.clone()));
    UART_DEV.set_state(DeviceState::Initialized);
    return Ok(());
}
