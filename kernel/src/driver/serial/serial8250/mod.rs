use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::{
        class::Class,
        device::{
            bus::Bus, device_manager, device_number::DeviceNumber, driver::Driver, Device,
            DeviceKObjType, DeviceState, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
        platform::{
            platform_device::{platform_device_manager, PlatformDevice},
            platform_driver::{platform_driver_manager, PlatformDriver},
        },
    },
    filesystem::kernfs::KernFSInode,
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};

use self::serial8250_pio::{send_to_serial8250_pio_com1, serial8250_pio_port_early_init};

use super::{uart_manager, UartDriver, UartPort};

mod serial8250_pio;

static mut SERIAL8250_ISA_DEVICES: Option<Arc<Serial8250ISADevices>> = None;
static mut SERIAL8250_ISA_DRIVER: Option<Arc<Serial8250ISADriver>> = None;

#[inline(always)]
#[allow(dead_code)]
fn serial8250_isa_devices() -> &'static Arc<Serial8250ISADevices> {
    unsafe { SERIAL8250_ISA_DEVICES.as_ref().unwrap() }
}

#[inline(always)]
#[allow(dead_code)]
fn serial8250_isa_driver() -> &'static Arc<Serial8250ISADriver> {
    unsafe { SERIAL8250_ISA_DRIVER.as_ref().unwrap() }
}

#[inline(always)]
pub(super) fn serial8250_manager() -> &'static Serial8250Manager {
    &Serial8250Manager
}

/// 标记serial8250是否已经初始化
static mut INITIALIZED: bool = false;

#[derive(Debug)]
pub(super) struct Serial8250Manager;

impl Serial8250Manager {
    /// 初始化串口设备（在内存管理初始化之前）
    pub fn early_init(&self) -> Result<(), SystemError> {
        // todo: riscv64: 串口设备初始化
        #[cfg(not(target_arch = "riscv64"))]
        serial8250_pio_port_early_init()?;
        return Ok(());
    }

    /// 初始化serial8250设备、驱动
    ///
    /// 应当在设备驱动模型初始化之后调用这里
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/tty/serial/8250/8250_core.c?r=&mo=30224&fi=1169#1169
    pub fn init(&self) -> Result<(), SystemError> {
        // 初始化serial8250 isa设备
        let serial8250_isa_dev = Serial8250ISADevices::new();
        unsafe {
            SERIAL8250_ISA_DEVICES = Some(serial8250_isa_dev.clone());
        }

        let serial8250_isa_driver = Serial8250ISADriver::new();
        unsafe {
            SERIAL8250_ISA_DRIVER = Some(serial8250_isa_driver.clone());
        }
        // todo: 把端口绑定到isa_dev、 isa_driver上
        self.register_ports(&serial8250_isa_driver, &serial8250_isa_dev);

        serial8250_isa_dev.set_driver(Some(Arc::downgrade(
            &(serial8250_isa_driver.clone() as Arc<dyn Driver>),
        )));
        // todo: 把驱动注册到uart层、tty层
        uart_manager().register_driver(&(serial8250_isa_driver.clone() as Arc<dyn UartDriver>))?;

        // 注册isa设备到platform总线
        platform_device_manager()
            .device_add(serial8250_isa_dev.clone() as Arc<dyn PlatformDevice>)
            .map_err(|e| {
                unsafe {
                    SERIAL8250_ISA_DEVICES = None;
                }
                return e;
            })?;

        // 把驱动注册到platform总线
        platform_driver_manager()
            .register(serial8250_isa_driver.clone() as Arc<dyn PlatformDriver>)?;

        unsafe {
            INITIALIZED = true;
        }

        return Ok(());
    }

    /// 把uart端口与uart driver、uart device绑定
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/tty/serial/8250/8250_core.c?r=&mo=30224&fi=1169#553
    fn register_ports(
        &self,
        uart_driver: &Arc<Serial8250ISADriver>,
        devs: &Arc<Serial8250ISADevices>,
    ) {
        self.bind_pio_ports(uart_driver, devs);
    }

    /// 把uart端口与uart driver绑定
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/tty/serial/serial_core.c?fi=uart_add_one_port#3048
    pub(self) fn uart_add_one_port(
        &self,
        _uart_driver: &Arc<Serial8250ISADriver>,
        _port: &dyn UartPort,
    ) -> Result<(), SystemError> {
        return Ok(());
        // todo!("Serial8250Manager::uart_add_one_port")
    }
}

/// 所有的8250串口设备都应该实现的trait
trait Serial8250Port: UartPort {
    fn device(&self) -> Option<Arc<Serial8250ISADevices>> {
        None
    }
    fn set_device(&self, device: Option<&Arc<Serial8250ISADevices>>);
}

#[derive(Debug)]
#[cast_to([sync] Device, PlatformDevice)]
struct Serial8250ISADevices {
    /// 设备id是否自动分配
    id_auto: AtomicBool,
    /// 平台设备id
    id: AtomicI32,

    inner: RwLock<InnerSerial8250ISADevices>,
    name: &'static str,
    kobj_state: LockedKObjectState,
}

impl Serial8250ISADevices {
    pub fn new() -> Arc<Self> {
        let r = Arc::new(Self {
            id_auto: AtomicBool::new(false),
            id: AtomicI32::new(Serial8250PlatformDeviceID::Legacy as i32),
            inner: RwLock::new(InnerSerial8250ISADevices::new()),
            name: "serial8250",
            kobj_state: LockedKObjectState::new(None),
        });

        device_manager().device_default_initialize(&(r.clone() as Arc<dyn Device>));

        return r;
    }
}

impl PlatformDevice for Serial8250ISADevices {
    fn compatible_table(&self) -> crate::driver::base::platform::CompatibleTable {
        unimplemented!()
    }

    fn pdev_id(&self) -> (i32, bool) {
        return (
            self.id.load(Ordering::SeqCst),
            self.id_auto.load(Ordering::SeqCst),
        );
    }

    fn set_pdev_id(&self, id: i32) {
        self.id.store(id, Ordering::SeqCst);
    }

    fn set_pdev_id_auto(&self, id_auto: bool) {
        self.id_auto.store(id_auto, Ordering::SeqCst);
    }

    fn pdev_name(&self) -> &str {
        return self.name;
    }

    fn is_initialized(&self) -> bool {
        return self.inner.read().device_state == DeviceState::Initialized;
    }

    fn set_state(&self, set_state: DeviceState) {
        self.inner.write().device_state = set_state;
    }
}

impl Device for Serial8250ISADevices {
    fn is_dead(&self) -> bool {
        false
    }
    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.read().bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.write().bus = bus;
    }

    fn dev_type(&self) -> DeviceType {
        DeviceType::Serial
    }

    fn id_table(&self) -> IdTable {
        return IdTable::new(self.name.to_string(), None);
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.read().driver.clone()?.upgrade()
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner.write().driver = driver;
    }

    fn can_match(&self) -> bool {
        self.inner.read().can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner.write().can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn set_class(&self, _class: Option<Arc<dyn Class>>) {
        todo!()
    }
}

impl KObject for Serial8250ISADevices {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.write().inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.read().inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.read().parent_kobj.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.write().parent_kobj = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.write().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        Some(&DeviceKObjType)
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {
        // 不允许修改
    }

    fn name(&self) -> String {
        self.name.to_string()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

#[derive(Debug)]
struct InnerSerial8250ISADevices {
    /// 当前设备所述的kset
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
    /// 当前设备所述的总线
    bus: Option<Weak<dyn Bus>>,
    inode: Option<Arc<KernFSInode>>,
    driver: Option<Weak<dyn Driver>>,
    device_state: DeviceState,
    can_match: bool,
}

impl InnerSerial8250ISADevices {
    fn new() -> Self {
        Self {
            kset: None,
            parent_kobj: None,
            bus: None,
            inode: None,
            driver: None,
            device_state: DeviceState::NotInitialized,
            can_match: false,
        }
    }
}

/// Serial 8250平台设备的id
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/serial_8250.h?fi=PLAT8250_DEV_LEGACY#49
#[derive(Debug)]
#[repr(i32)]
enum Serial8250PlatformDeviceID {
    Legacy = -1,
}

#[derive(Debug)]

struct InnerSerial8250ISADriver {
    bus: Option<Weak<dyn Bus>>,
    kobj_type: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
    kern_inode: Option<Arc<KernFSInode>>,
    devices: Vec<Arc<dyn Device>>,
}

impl InnerSerial8250ISADriver {
    fn new() -> Self {
        Self {
            bus: None,
            kobj_type: None,
            kset: None,
            parent_kobj: None,
            kern_inode: None,
            devices: Vec::new(),
        }
    }
}

#[derive(Debug)]
#[cast_to([sync] Driver, PlatformDriver)]
#[allow(dead_code)]
struct Serial8250ISADriver {
    inner: RwLock<InnerSerial8250ISADriver>,
    name: &'static str,
    kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
}

impl Serial8250ISADriver {
    pub fn new() -> Arc<Self> {
        let r = Arc::new(Self {
            inner: RwLock::new(InnerSerial8250ISADriver::new()),
            name: "serial8250",
            kobj_state: LockedKObjectState::new(None),
            self_ref: Weak::default(),
        });

        unsafe {
            let p = r.as_ref() as *const Self as *mut Self;
            (*p).self_ref = Arc::downgrade(&r);
        }

        return r;
    }
}

impl UartDriver for Serial8250ISADriver {
    fn device_number(&self) -> DeviceNumber {
        todo!()
    }

    fn max_devs_num(&self) -> i32 {
        todo!()
    }
}

impl PlatformDriver for Serial8250ISADriver {
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        let isa_dev = device
            .clone()
            .arc_any()
            .downcast::<Serial8250ISADevices>()
            .map_err(|_| {
                kerror!("Serial8250ISADriver::probe: device is not a Serial8250ISADevices");
                SystemError::EINVAL
            })?;
        isa_dev.set_driver(Some(self.self_ref.clone()));

        return Ok(());
    }

    fn remove(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn suspend(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn resume(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }
}

impl Driver for Serial8250ISADriver {
    fn id_table(&self) -> Option<IdTable> {
        None
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner.read().devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        self.inner.write().devices.push(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let mut inner = self.inner.write();

        inner.devices.retain(|d| !Arc::ptr_eq(d, device));
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.read().bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.write().bus = bus;
    }
}

impl KObject for Serial8250ISADriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.write().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.read().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.read().parent_kobj.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.write().parent_kobj = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.write().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.read().kobj_type.clone()
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.write().kobj_type = ktype;
    }

    fn name(&self) -> String {
        "serial8250".to_string()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

/// 临时函数，用于向默认的串口发送数据
pub fn send_to_default_serial8250_port(s: &[u8]) {
    #[cfg(target_arch = "x86_64")]
    send_to_serial8250_pio_com1(s);

    #[cfg(target_arch = "riscv64")]
    {
        if unsafe { INITIALIZED } {
            todo!("riscv64: send_to_default_serial8250_port")
        } else {
            crate::arch::driver::sbi::console_putstr(s);
        }
    }
}
