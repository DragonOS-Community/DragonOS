use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use ida::IdAllocator;
use intertrait::cast::CastArc;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        device::{
            bus::{bus_manager, Bus},
            device_manager,
            driver::{driver_manager, Driver},
            Device,
        },
        kobject::KObject,
        subsys::SubSysPrivate,
    },
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport, SYSFS_ATTR_MODE_RO,
        },
        vfs::syscall::ModeType,
    },
    init::initcall::INITCALL_CORE,
};

use super::{VirtIODevice, VirtIODeviceIndex, VirtIODriver};

static mut VIRTIO_BUS: Option<Arc<VirtIOBus>> = None;

#[inline(always)]
pub fn virtio_bus() -> Arc<VirtIOBus> {
    unsafe { VIRTIO_BUS.as_ref().unwrap().clone() }
}

#[derive(Debug)]
pub struct VirtIOBus {
    private: SubSysPrivate,
}

impl VirtIOBus {
    pub fn new() -> Arc<Self> {
        let w: Weak<Self> = Weak::new();
        let private = SubSysPrivate::new("virtio".to_string(), Some(w), None, &[]);
        let bus = Arc::new(Self { private });
        bus.subsystem()
            .set_bus(Some(Arc::downgrade(&(bus.clone() as Arc<dyn Bus>))));

        return bus;
    }
}

impl Bus for VirtIOBus {
    fn name(&self) -> String {
        self.private.name()
    }

    fn dev_name(&self) -> String {
        return self.name();
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        // todo: VirtIODeviceAttrGroup
        return &[];
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.private;
    }

    fn probe(&self, device: &Arc<dyn Device>) -> Result<(), SystemError> {
        let drv = device.driver().ok_or(SystemError::EINVAL)?;
        let virtio_drv = drv.cast::<dyn VirtIODriver>().map_err(|_| {
            kerror!(
                "VirtIOBus::probe() failed: device.driver() is not a VirtioDriver. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;

        let virtio_dev = device.clone().cast::<dyn VirtIODevice>().map_err(|_| {
            kerror!(
                "VirtIOBus::probe() failed: device is not a VirtIODevice. Device: '{:?}'",
                device.name()
            );
            SystemError::EINVAL
        })?;

        return virtio_drv.probe(&virtio_dev);
    }

    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn sync_state(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn Device>) {
        todo!()
    }

    fn resume(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!()
    }

    fn match_device(
        &self,
        _device: &Arc<dyn Device>,
        _driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        // todo: https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/virtio/virtio.c#85
        todo!("VirtIOBus::match_device() is not implemented")
    }
}

#[unified_init(INITCALL_CORE)]
fn virtio_init() -> Result<(), SystemError> {
    let bus = VirtIOBus::new();
    unsafe {
        VIRTIO_BUS = Some(bus.clone());
    }
    bus_manager()
        .register(bus)
        .expect("Failed to register virtio bus!");
    Ok(())
}

#[inline(always)]
pub fn virtio_driver_manager() -> &'static VirtIODriverManager {
    &VirtIODriverManager
}

pub struct VirtIODriverManager;

impl VirtIODriverManager {
    pub fn register(&self, driver: Arc<dyn VirtIODriver>) -> Result<(), SystemError> {
        driver.set_bus(Some(Arc::downgrade(&(virtio_bus() as Arc<dyn Bus>))));
        return driver_manager().register(driver as Arc<dyn Driver>);
    }

    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn VirtIODriver>) {
        driver_manager().unregister(&(driver.clone() as Arc<dyn Driver>));
    }
}

#[inline(always)]
pub fn virtio_device_manager() -> &'static VirtIODeviceManager {
    &VirtIODeviceManager
}

pub struct VirtIODeviceManager;

impl VirtIODeviceManager {
    pub fn device_add(&self, dev: Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        dev.set_bus(Some(Arc::downgrade(&(virtio_bus() as Arc<dyn Bus>))));
        device_manager().device_default_initialize(&(dev.clone() as Arc<dyn Device>));
        let drv = dev.driver().ok_or(SystemError::EINVAL)?;

        let virtio_drv = drv.cast::<dyn VirtIODriver>().map_err(|_| {
            kerror!(
                "VirtIODeviceManager::device_add() failed: device.driver() is not a VirtioDriver. Device: '{:?}'",
                dev.name()
            );
            SystemError::EINVAL
        })?;
        let virtio_index = VIRTIO_DEVICE_INDEX_MANAGER.alloc();
        dev.set_virtio_device_index(virtio_index);
        dev.set_device_name(format!("virtio{}", virtio_index.data()));
        virtio_drv.probe(&dev)?;

        device_manager().add_device(dev.clone() as Arc<dyn Device>)?;
        device_manager().add_groups(&(dev as Arc<dyn Device>), &[&VirtIODeviceAttrGroup])
    }

    #[allow(dead_code)]
    pub fn device_remove(&self, dev: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        device_manager().remove(&(dev.clone() as Arc<dyn Device>));
        return Ok(());
    }
}

static VIRTIO_DEVICE_INDEX_MANAGER: VirtIODeviceIndexManager = VirtIODeviceIndexManager::new();

/// VirtIO设备索引管理器
///
/// VirtIO设备索引管理器用于分配和管理VirtIO设备的唯一索引。
pub struct VirtIODeviceIndexManager {
    // ID分配器
    ///
    /// ID分配器用于分配唯一的索引给VirtIO设备。
    ida: IdAllocator,
}

// VirtIO设备索引管理器的新建实例
impl VirtIODeviceIndexManager {
    /// 创建新的VirtIO设备索引管理器实例
    ///
    /// 创建一个新的VirtIO设备索引管理器实例，初始时分配器从0开始，直到最大usize值。
    const fn new() -> Self {
        Self {
            ida: IdAllocator::new(0, usize::MAX),
        }
    }

    /// 分配一个新的VirtIO设备索引
    ///
    /// 分配一个唯一的索引给VirtIO设备。
    pub fn alloc(&self) -> VirtIODeviceIndex {
        VirtIODeviceIndex(self.ida.alloc().unwrap())
    }

    // 释放一个VirtIO设备索引
    ///
    /// 释放之前分配的VirtIO设备索引，使其可以被重新使用。
    #[allow(dead_code)]
    pub fn free(&self, index: VirtIODeviceIndex) {
        self.ida.free(index.0);
    }
}

/// VirtIO设备属性组
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/drivers/virtio/virtio.c#64
#[derive(Debug)]
pub struct VirtIODeviceAttrGroup;

impl AttributeGroup for VirtIODeviceAttrGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&AttrDevice, &AttrVendor]
    }
}

#[derive(Debug)]
struct AttrDevice;

impl Attribute for AttrDevice {
    fn name(&self) -> &str {
        "device"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = kobj.cast::<dyn VirtIODevice>().map_err(|_| {
            kerror!("AttrDevice::show() failed: kobj is not a VirtIODevice");
            SystemError::EINVAL
        })?;
        let device_type_id = dev.device_type_id();

        return sysfs_emit_str(buf, &format!("0x{:04x}\n", device_type_id));
    }
}

#[derive(Debug)]
struct AttrVendor;

impl Attribute for AttrVendor {
    fn name(&self) -> &str {
        "vendor"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = kobj.cast::<dyn VirtIODevice>().map_err(|_| {
            kerror!("AttrVendor::show() failed: kobj is not a VirtIODevice");
            SystemError::EINVAL
        })?;
        let vendor = dev.vendor();

        return sysfs_emit_str(buf, &format!("0x{:04x}\n", vendor));
    }
}
