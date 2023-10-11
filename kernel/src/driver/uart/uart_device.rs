use crate::{
    driver::{
        base::{
            char::CharDevice,
            device::{
                bus::Bus, driver::DriverError, Device, DeviceError, DeviceNumber,
                DevicePrivateData, DeviceResource, DeviceState, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectState},
            kset::KSet,
            platform::{
                platform_device::PlatformDevice, platform_driver::PlatformDriver, CompatibleTable,
            },
        },
        Driver,
    },
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        kernfs::KernFSInode,
        vfs::{
            syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode, Metadata,
            PollStatus,
        },
    },
    include::bindings::bindings::{io_in8, io_out8},
    kinfo,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
    syscall::SystemError,
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{
    any::Any,
    char,
    intrinsics::offset,
    str::{self, from_utf8},
};

const UART_SUCCESS: i32 = 0;
const E_UART_BITS_RATE_ERROR: i32 = 1;
const E_UART_SERIAL_FAULT: i32 = 2;
const UART_MAX_BITS_RATE: u32 = 115200;

lazy_static! {
    // 串口设备
    pub static ref UART_DEV: Arc<LockedUart> = Arc::new(LockedUart::default());
    // 串口驱动
    pub static ref UART_DRV: Arc<LockedUartDriver> = Arc::new(LockedUartDriver::default());
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
    private_data: DevicePrivateData, // 设备状态
    fs: Weak<DevFS>,                 // 文件系统
    port: UartPort,
    baud_rate: u32,
    metadata: Metadata,
}

impl Default for Uart {
    fn default() -> Self {
        let mut metadata = Metadata::default();
        metadata.file_type = FileType::CharDevice;
        c_uart_init(UartPort::COM1.to_u16(), 115200);
        Self {
            private_data: DevicePrivateData::new(
                IdTable::new(
                    "uart".to_string(),
                    DeviceNumber::new(DeviceNumber::from_major_minor(4, 64)),
                ),
                None,
                CompatibleTable::new(vec!["uart"]),
                DeviceState::NotInitialized,
            ),
            fs: Weak::default(),
            port: UartPort::COM1,
            baud_rate: 115200,
            metadata,
        }
    }
}

// @brief 串口设备结构体(加锁)
#[derive(Debug)]
pub struct LockedUart(SpinLock<Uart>);

impl Default for LockedUart {
    fn default() -> Self {
        Self(SpinLock::new(Uart::default()))
    }
}

impl KObject for LockedUart {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_inode(&self, _inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: KObjectState) {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, _name: alloc::string::String) {
        todo!()
    }

    fn set_kset(&self, _kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn set_parent(&self, _parent: Option<Weak<dyn KObject>>) {
        todo!()
    }
}

impl PlatformDevice for LockedUart {
    fn is_initialized(&self) -> bool {
        let state = self.0.lock().private_data.state();
        match state {
            DeviceState::Initialized => true,
            _ => false,
        }
    }

    fn set_state(&self, set_state: DeviceState) {
        self.0.lock().private_data.set_state(set_state);
    }

    fn compatible_table(&self) -> CompatibleTable {
        return self.0.lock().private_data.compatible_table().clone();
    }
}

impl Device for LockedUart {
    fn id_table(&self) -> IdTable {
        return IdTable::new(
            "uart".to_string(),
            DeviceNumber::new(DeviceNumber::from_major_minor(4, 64)),
        );
    }

    fn dev_type(&self) -> DeviceType {
        DeviceType::Serial
    }

    fn bus(&self) -> Option<Arc<dyn Bus>> {
        todo!("LockedUart::bus()")
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        todo!("LockedUart::driver()")
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, _driver: Option<Arc<dyn Driver>>) {
        todo!("LockedUart::set_driver()")
    }
}

impl CharDevice for LockedUart {
    fn read(&self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let device = self.0.lock();
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }
        kinfo!("------len:  {:?}", len);
        for i in 0..len {
            buf[i] = Self::uart_read_byte(&device.port) as u8;
            kinfo!("------buf[{:?}] = {:?}", i, buf[i]);
        }
        return Ok(len);
    }

    fn write(&self, len: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let device = self.0.lock();
        if len > buf.len() {
            return Err(SystemError::E2BIG);
        }
        Self::uart_send(
            &device.port,
            from_utf8(&buf[0..len]).map_err(|_| SystemError::EIO)?,
        );

        return Ok(len);
    }

    fn sync(&self) -> Result<(), SystemError> {
        todo!()
    }
}

// impl TtyDevice for LockedUart {
//     fn ioctl(&self, cmd: String) -> Result<(), DeviceError> {
//         //TODO 补充详细信息
//         Err(DeviceError::UnsupportedOperation)
//     }
//     fn state(&self) -> Result<TtyState, TtyError> {
//         todo!()
//     }
// }

impl IndexNode for LockedUart {
    fn read_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        CharDevice::read(self, len, buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        len: usize,
        buf: &[u8],
        _data: &mut FilePrivateData,
    ) -> Result<usize, SystemError> {
        CharDevice::write(self, len, buf)
    }

    fn poll(&self) -> Result<PollStatus, SystemError> {
        todo!()
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        return self
            .0
            .lock()
            .fs
            .clone()
            .upgrade()
            .expect("DevFS is not initialized inside Uart Device");
    }

    fn as_any_ref(&self) -> &dyn Any {
        todo!()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        return Ok(self.0.lock().metadata.clone());
    }

    fn open(
        &self,
        _data: &mut FilePrivateData,
        _mode: &crate::filesystem::vfs::file::FileMode,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Ok(());
    }

    fn close(&self, _data: &mut FilePrivateData) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Ok(());
    }

    fn set_metadata(&self, _metadata: &Metadata) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Ok(self.0.lock().metadata = _metadata.clone());
    }
    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则默认调用其create_with_data方法。如果仍未实现，则会得到一个Err(-EOPNOTSUPP_OR_ENOTSUP)的返回值
        return self.create_with_data(name, file_type, mode, 0);
    }

    fn create_with_data(
        &self,
        _name: &str,
        _file_type: FileType,
        _mode: ModeType,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn link(&self, _name: &str, _other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn unlink(&self, _name: &str) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn rmdir(&self, _name: &str) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn move_(
        &self,
        _old_name: &str,
        _target: &Arc<dyn IndexNode>,
        _new_name: &str,
    ) -> Result<(), SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn get_entry_name(&self, _ino: crate::filesystem::vfs::InodeId) -> Result<String, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn get_entry_name_and_metadata(
        &self,
        ino: crate::filesystem::vfs::InodeId,
    ) -> Result<(String, Metadata), SystemError> {
        // 如果有条件，请在文件系统中使用高效的方式实现本接口，而不是依赖这个低效率的默认实现。
        let name = self.get_entry_name(ino)?;
        let entry = self.find(&name)?;
        return Ok((name, entry.metadata()?));
    }

    fn ioctl(&self, _cmd: u32, _data: usize) -> Result<usize, SystemError> {
        // 若文件系统没有实现此方法，则返回“不支持”
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn mount(
        &self,
        _fs: Arc<dyn FileSystem>,
    ) -> Result<Arc<crate::filesystem::vfs::MountFS>, SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn truncate(&self, _len: usize) -> Result<(), SystemError> {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }

    fn sync(&self) -> Result<(), SystemError> {
        return Ok(());
    }
}

impl DeviceINode for LockedUart {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.0.lock().fs = fs;
    }
}

impl LockedUart {
    /// @brief 串口初始化
    /// @param uart_port 端口号
    /// @param baud_rate 波特率
    /// @return 初始化成功，返回0,失败，返回错误信息
    #[allow(dead_code)]
    pub fn uart_init(uart_port: &UartPort, baud_rate: u32) -> Result<(), DeviceError> {
        let message: &'static str = "uart init.";
        let port = uart_port.to_u16();
        // 错误的比特率
        if baud_rate > UART_MAX_BITS_RATE || UART_MAX_BITS_RATE % baud_rate != 0 {
            return Err(DeviceError::InitializeFailed);
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
                return Err(DeviceError::InitializeFailed);
            }

            // If serial is not faulty set it in normal operation mode
            // (not-loopback with IRQs enabled and OUT#1 and OUT#2 bits enabled)
            io_out8(port + 4, 0x08);
        }
        Self::uart_send(uart_port, message);
        Ok(())
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
        while Self::is_transmit_empty(port) == false {} //TODO:pause
        for c in s.bytes() {
            unsafe {
                io_out8(port, c);
            }
        }
    }

    /// @brief 串口接收一个字节
    /// @param uart_port 端口号
    /// @return 接收的字节
    #[allow(dead_code)]
    fn uart_read_byte(uart_port: &UartPort) -> char {
        let port = uart_port.to_u16();
        while Self::serial_received(port) == false {} //TODO:pause
        return unsafe { io_in8(port) as char };
    }

    #[allow(dead_code)]
    fn port() -> u16 {
        UartPort::COM1.to_u16()
    }
}

// @brief 串口驱动结构体
#[repr(C)]
#[derive(Debug)]
pub struct UartDriver {
    id_table: IdTable,

    sys_info: Option<Arc<dyn IndexNode>>,
}

impl Default for UartDriver {
    fn default() -> Self {
        Self {
            id_table: IdTable::new(
                "ttyS".to_string(),
                DeviceNumber::new(DeviceNumber::from_major_minor(4, 64)),
            ),

            sys_info: None,
        }
    }
}

// @brief 串口驱动结构体(加锁)
#[derive(Debug)]
pub struct LockedUartDriver(SpinLock<UartDriver>);

impl Default for LockedUartDriver {
    fn default() -> Self {
        Self(SpinLock::new(UartDriver::default()))
    }
}

impl KObject for LockedUartDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_inode(&self, _inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, _state: KObjectState) {
        todo!()
    }

    fn name(&self) -> alloc::string::String {
        todo!()
    }

    fn set_name(&self, _name: alloc::string::String) {
        todo!()
    }

    fn set_kset(&self, _kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn set_parent(&self, _parent: Option<Weak<dyn KObject>>) {
        todo!()
    }
}

impl Driver for LockedUartDriver {
    fn id_table(&self) -> IdTable {
        return IdTable::new("uart_driver".to_string(), DeviceNumber::new(0));
    }

    fn probe(&self, data: &DevicePrivateData) -> Result<(), DriverError> {
        let table = data.compatible_table();
        if table.matches(&CompatibleTable::new(vec!["uart"])) {
            return Ok(());
        }
        return Err(DriverError::ProbeError);
    }

    fn load(
        &self,
        _data: DevicePrivateData,
        _resource: Option<DeviceResource>,
    ) -> Result<Arc<dyn Device>, DriverError> {
        return Err(DriverError::UnsupportedOperation);
    }
}

impl LockedUartDriver {
    /// @brief 创建串口驱动
    /// @param sys_info: sys文件系统inode
    /// @return  
    #[allow(dead_code)]
    pub fn new(sys_info: Option<Arc<dyn IndexNode>>) -> Self {
        Self(SpinLock::new(UartDriver::new(sys_info)))
    }
}

impl PlatformDriver for LockedUartDriver {
    fn compatible_table(&self) -> CompatibleTable {
        return CompatibleTable::new(vec!["uart"]);
    }
}

impl UartDriver {
    /// @brief 创建串口驱动
    /// @param sys_info: sys文件系统inode
    /// @return 返回串口驱动
    #[allow(dead_code)]
    pub fn new(sys_info: Option<Arc<dyn IndexNode>>) -> Self {
        Self {
            id_table: IdTable::new(
                "ttyS".to_string(),
                DeviceNumber::new(DeviceNumber::from_major_minor(4, 64)),
            ),
            sys_info,
        }
    }
}

///@brief 发送数据
///@param port 端口号
///@param c 要发送的数据
#[no_mangle]
pub extern "C" fn c_uart_send(port: u16, c: u8) {
    while LockedUart::is_transmit_empty(port) == false {} //TODO:pause
    unsafe {
        io_out8(port, c);
    }
}

///@brief 从uart接收数据
///@param port 端口号
///@return u8 接收到的数据
#[no_mangle]
pub extern "C" fn c_uart_read(port: u16) -> u8 {
    while LockedUart::serial_received(port) == false {} //TODO:pause
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
    // 以后设备管理初始化完善后不应该出现这种代码，应该在 Driver load 一个设备，即返回设备实例之前就完成设备的 init ，不应该用 lazy_init 在设备上
    let dev = UART_DEV.0.lock();
    LockedUart::uart_init(&dev.port, dev.baud_rate).map_err(|_| SystemError::ENODEV)?;
    drop(dev);
    // let device_inode = bus_device_register("platform:0", &UART_DEV.id_table().name())
    //     .expect("uart device register error");
    // UART_DEV.set_sys_info(Some(device_inode));
    // let driver_inode = bus_driver_register("platform:0", &UART_DRV.id_table().name())
    //     .expect("uart driver register error");
    // UART_DRV.set_sys_info(Some(driver_inode));
    UART_DEV.set_state(DeviceState::Initialized);
    devfs_register(&UART_DEV.id_table().name(), UART_DEV.clone())?;
    // DEVICE_MANAGER.add_device(UART_DEV.id_table().clone(), UART_DEV.clone());
    return Ok(());
}
