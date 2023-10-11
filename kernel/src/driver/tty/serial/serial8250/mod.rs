use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicI32, Ordering},
};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};

use crate::{
    driver::{
        base::{
            device::{bus::Bus, Device, DeviceKObjType, DeviceNumber, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectState},
            kset::KSet,
            platform::{
                platform_device::{platform_device_manager, PlatformDevice},
                platform_driver::{platform_driver_manager, PlatformDriver},
            },
        },
        tty::{
            tty_device::TtyDevice,
            tty_driver::{TtyDriver, TtyDriverMetadata, TtyDriverOperations},
        },
        Driver,
    },
    filesystem::kernfs::KernFSInode,
    libs::rwlock::{RwLockReadGuard, RwLockWriteGuard},
    syscall::SystemError,
};

use self::serial8250_pio::{send_to_serial8250_pio_com1, serial8250_pio_port_early_init};

use super::{UartDriver, UartPort};

mod serial8250_pio;

static mut SERIAL8250_ISA_DEVICES: Option<Arc<Serial8250ISADevices>> = None;
static mut SERIAL8250_ISA_DRIVER: Option<Arc<Serial8250ISADriver>> = None;

#[inline(always)]
fn serial8250_isa_devices() -> &'static Arc<Serial8250ISADevices> {
    unsafe { SERIAL8250_ISA_DEVICES.as_ref().unwrap() }
}

#[inline(always)]
fn serial8250_isa_driver() -> &'static Arc<Serial8250ISADriver> {
    unsafe { SERIAL8250_ISA_DRIVER.as_ref().unwrap() }
}

#[inline(always)]
pub(self) fn serial8250_manager() -> &'static Serial8250Manager {
    &Serial8250Manager
}

#[derive(Debug)]
pub(super) struct Serial8250Manager;

impl Serial8250Manager {
    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/tty/serial/8250/8250_core.c?r=&mo=30224&fi=1169#553
    fn register_ports(
        &self,
        uart_driver: &Arc<Serial8250ISADriver>,
        devs: &Arc<Serial8250ISADevices>,
    ) {
        todo!("Serial8250Manager::register_ports")
    }
}

/// 所有的8250串口设备都应该实现的trait
pub trait Serial8250Port: UartPort {}

/// 初始化串口设备（在内存管理初始化之前）
pub fn serial8250_init_stage1() -> Result<(), SystemError> {
    serial8250_pio_port_early_init()?;

    return Ok(());
}

/// 初始化uart设备
///
/// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/tty/serial/8250/8250_core.c?r=&mo=30224&fi=1169#1169
pub fn serial8250_init() -> Result<(), SystemError> {
    // todo: 初始化serial8250设备

    let serial8250_isa_dev = Arc::new(Serial8250ISADevices::new());
    unsafe {
        SERIAL8250_ISA_DEVICES = Some(serial8250_isa_dev.clone());
    }
    let serial8250_isa_driver = Serial8250ISADriver::new();
    unsafe {
        SERIAL8250_ISA_DRIVER = Some(serial8250_isa_driver.clone());
    }

    // todo: 初始化端口

    // todo: 把端口绑定到isa_dev上

    // todo: 把驱动注册到uart层、tty层

    // 注册设备到platform总线

    platform_device_manager()
        .device_add(serial8250_isa_dev.clone() as Arc<dyn PlatformDevice>)
        .map_err(|e| {
            unsafe {
                SERIAL8250_ISA_DEVICES = None;
            }
            return e;
        })?;

    // todo: 把驱动注册到platform总线
    platform_driver_manager().register(serial8250_isa_driver.clone() as Arc<dyn PlatformDriver>)?;

    return Ok(());
}

#[derive(Debug)]
struct Serial8250ISADevices {
    /// 设备id是否自动分配
    id_auto: AtomicBool,
    /// 平台设备id
    id: AtomicI32,
}

impl Serial8250ISADevices {
    pub fn new() -> Self {
        return Self {
            id_auto: AtomicBool::new(false),
            id: AtomicI32::new(Serial8250PlatformDeviceID::Legacy as i32),
        };
    }
}

impl PlatformDevice for Serial8250ISADevices {
    fn compatible_table(&self) -> crate::driver::base::platform::CompatibleTable {
        unimplemented!()
    }

    fn pdev_id(&self) -> Option<(i32, bool)> {
        return Some((
            self.id.load(Ordering::SeqCst),
            self.id_auto.load(Ordering::SeqCst),
        ));
    }

    fn set_pdev_id(&self, id: i32) {
        self.id.store(id, Ordering::SeqCst);
    }

    fn set_pdev_id_auto(&self, id_auto: bool) {
        self.id_auto.store(id_auto, Ordering::SeqCst);
    }

    fn name(&self) -> &str {
        return "serial8250";
    }

    fn is_initialized(&self) -> bool {
        unimplemented!()
    }

    fn set_state(&self, set_state: crate::driver::base::device::DeviceState) {
        unimplemented!()
    }
}

impl Device for Serial8250ISADevices {
    fn is_dead(&self) -> bool {
        false
    }
    fn bus(&self) -> Option<Arc<dyn Bus>> {
        todo!()
    }

    fn dev_type(&self) -> DeviceType {
        todo!()
    }

    fn id_table(&self) -> IdTable {
        todo!()
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        todo!()
    }

    fn set_driver(&self, driver: Option<Arc<dyn Driver>>) {
        todo!()
    }
}

impl KObject for Serial8250ISADevices {
    fn as_any_ref(&self) -> &dyn Any {
        todo!()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        Some(&DeviceKObjType)
    }

    fn name(&self) -> String {
        todo!()
    }

    fn set_name(&self, name: String) {
        todo!()
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        todo!()
    }
}

/// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/include/linux/serial_8250.h?fi=PLAT8250_DEV_LEGACY#49
#[derive(Debug)]
#[repr(i32)]
enum Serial8250PlatformDeviceID {
    Legacy = -1,
}

#[derive(Debug)]
struct Serial8250ISADriver {}

impl Serial8250ISADriver {
    pub fn new() -> Arc<Self> {
        return Arc::new(Self {});
    }
}

impl TtyDriver for Serial8250ISADriver {
    fn driver_name(&self) -> &str {
        todo!()
    }

    fn dev_name(&self) -> &str {
        todo!()
    }

    fn metadata(&self) -> &TtyDriverMetadata {
        todo!()
    }

    fn other(&self) -> Option<&Arc<dyn TtyDriver>> {
        todo!()
    }

    fn ttys(&self) -> &[Arc<TtyDevice>] {
        todo!()
    }

    fn tty_ops(&self) -> Option<&'static dyn TtyDriverOperations> {
        None
    }
}

impl UartDriver for Serial8250ISADriver {
    fn device_number(&self) -> DeviceNumber {
        todo!()
    }

    fn devs_num(&self) -> i32 {
        todo!()
    }
}

impl PlatformDriver for Serial8250ISADriver {
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn remove(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn shutdown(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn suspend(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn resume(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }
}

impl Driver for Serial8250ISADriver {
    fn probe(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn remove(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn sync_state(&self, device: &Arc<dyn Device>) {
        todo!()
    }

    fn shutdown(&self, device: &Arc<dyn Device>) {
        todo!()
    }

    fn resume(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn id_table(&self) -> IdTable {
        todo!()
    }

    fn devices(&self) -> alloc::vec::Vec<Arc<dyn Device>> {
        todo!()
    }
}

impl KObject for Serial8250ISADriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        todo!()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        todo!()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        todo!()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        todo!()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        todo!()
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        todo!()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        todo!()
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        todo!()
    }

    fn name(&self) -> String {
        "serial8250".to_string()
    }

    fn set_name(&self, name: String) {}

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        todo!()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        todo!()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        todo!()
    }
}

/// 临时函数，用于向默认的串口发送数据
pub fn send_to_default_serial8250_port(s: &[u8]) {
    send_to_serial8250_pio_com1(s);
}
