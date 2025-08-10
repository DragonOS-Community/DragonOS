use core::any::Any;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, driver::Driver, Device, DeviceCommonData, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        pci::{dev_id::PciDeviceID, device::PciDevice, pci_irq::IrqType},
    },
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{
            file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport, SYSFS_ATTR_MODE_RO,
        },
        vfs::syscall::ModeType,
    },
    libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
};
#[derive(Debug)]
#[cast_to([sync] Device)]
#[cast_to([sync] PciDevice)]
/// # 结构功能
/// 这是一个测试用的PciDevice，也可以作为新PciDevice的参考
/// 它需要实现KObject PciDevice Device这些接口
/// 并通过函数pci_device_manager().device_add（）来将设备进行接入
pub struct TestDevice {
    device_data: RwLock<DeviceCommonData>,
    kobj_data: RwLock<KObjectCommonData>,
    kobj_state: LockedKObjectState,
    static_type: RwLock<IrqType>,
}

impl TestDevice {
    pub fn new() -> Self {
        let common_dev = RwLock::new(DeviceCommonData::default());
        let common_kobj = RwLock::new(KObjectCommonData::default());
        Self {
            device_data: common_dev,
            kobj_data: common_kobj,
            kobj_state: LockedKObjectState::new(None),
            static_type: RwLock::new(IrqType::Unused),
        }
    }
}

impl PciDevice for TestDevice {
    fn dynid(&self) -> PciDeviceID {
        PciDeviceID::dummpy()
    }

    fn vendor(&self) -> u16 {
        return 0xffff;
    }

    fn device_id(&self) -> u16 {
        return 0xffff;
    }

    fn subsystem_vendor(&self) -> u16 {
        return 0xffff;
    }

    fn subsystem_device(&self) -> u16 {
        return 0xffff;
    }

    fn class_code(&self) -> u8 {
        return 0xff;
    }

    fn irq_line(&self) -> u8 {
        return 0xff;
    }

    fn revision(&self) -> u8 {
        return 0xff;
    }

    fn irq_type(&self) -> &RwLock<crate::driver::pci::pci_irq::IrqType> {
        return &self.static_type;
    }

    fn interface_code(&self) -> u8 {
        return 0xff;
    }

    fn subclass(&self) -> u8 {
        return 0xff;
    }
}

impl Device for TestDevice {
    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&HelloAttr])
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.device_data.read().bus.clone()
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.device_data.write();
        let r = guard.class.clone()?.upgrade();
        if r.is_none() {
            guard.class = None;
        }

        return r;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.device_data.read().driver.clone()?.upgrade()
    }

    fn dev_type(&self) -> DeviceType {
        DeviceType::Pci
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("testPci".to_string(), None)
    }

    fn can_match(&self) -> bool {
        true
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.device_data.write().bus = bus
    }

    fn set_can_match(&self, _can_match: bool) {
        //todo
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.device_data.write().class = class
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.device_data.write().driver = driver
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.device_data.read().parent.clone()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.device_data.write().parent = dev_parent
    }
}

impl KObject for TestDevice {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.kobj_data.write().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.kobj_data.read().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.kobj_data.read().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.kobj_data.write().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.kobj_data.read().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.kobj_data.write().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.kobj_data.read().kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.kobj_data.write().kobj_type = ktype;
    }

    fn name(&self) -> String {
        "PciTest".to_string()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

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
pub struct HelloAttr;

impl AttributeGroup for HelloAttr {
    fn name(&self) -> Option<&str> {
        return Some("TestAttr");
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&Hello]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        return Some(attr.mode());
    }
}
#[derive(Debug)]
pub struct Hello;

impl Attribute for Hello {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RO
    }

    fn name(&self) -> &str {
        "Hello"
    }

    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        return sysfs_emit_str(_buf, "Hello Pci");
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}
