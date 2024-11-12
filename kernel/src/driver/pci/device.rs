use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::base::{
        device::{
            bus::Bus, device_manager, driver::Driver, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLock, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{
    dev_id::PciDeviceID,
    pci_irq::IrqType,
    subsys::{pci_bus, pci_bus_device},
};

/// # 结构功能
/// 该结构为Pci设备的管理器，使用该结构可以将pci设备添加到sysfs中
pub struct PciDeviceManager;

pub fn pci_device_manager() -> &'static PciDeviceManager {
    &PciDeviceManager
}

impl PciDeviceManager {
    /// #函数的功能
    /// 将pci设备注册到sysfs中
    ///
    /// ## 参数：
    /// - 'pci_dev':需要添加的pci设备
    ///
    /// ## 返回值：
    /// - OK(()) :表示成功
    /// - Err(e) :失败原因
    pub fn device_add(&self, pci_dev: Arc<dyn PciDevice>) -> Result<(), SystemError> {
        // pci设备一般放置在/sys/device/pci:xxxx下
        if pci_dev.dev_parent().is_none() {
            pci_dev.set_dev_parent(Some(Arc::downgrade(&(pci_bus_device() as Arc<dyn Device>))));
        }
        // 设置设备的总线
        pci_dev.set_bus(Some(Arc::downgrade(&(pci_bus() as Arc<dyn Bus>))));
        // 对设备进行默认的初始化
        device_manager().device_default_initialize(&(pci_dev.clone() as Arc<dyn Device>));
        // 使用设备管理器注册设备，当设备被注册后，会根据它的总线字段，在对应的总线上扫描驱动，并尝试进行匹配
        let r = device_manager().add_device(pci_dev.clone() as Arc<dyn Device>);

        if r.is_ok() {
            //todo:这里可能还要处理一些设置成功后设备状态的变化
            return Ok(());
        } else {
            //todo:这里可能有一些添加失败的处理
            return r;
        }
    }
}

/// #trait功能
/// 要进入sysfs的Pci设备应当实现的trait
pub trait PciDevice: Device {
    /// # 函数的功能
    /// 返回本设备的PciDeviceID，该ID用于driver和device之间的匹配
    ///
    /// ## 返回值
    /// - 'PciDeviceID' :本设备的PciDeviceID
    fn dynid(&self) -> PciDeviceID;

    /// # 函数的功能
    /// 返回本设备的供应商（vendor）ID
    ///
    /// ## 返回值
    /// - u16 :表示供应商ID
    fn vendor(&self) -> u16;
    fn device_id(&self) -> u16;
    fn subsystem_vendor(&self) -> u16;
    fn subsystem_device(&self) -> u16;
    fn revision(&self) -> u8;
    fn class_code(&self) -> u8;
    fn irq_type(&self) -> &RwLock<IrqType>;
    fn irq_line(&self) -> u8;
    fn subclass(&self) -> u8;
    fn interface_code(&self) -> u8;
}

/// #结构功能
/// 由于Pci总线本身就属于一个设备，故该结构代表Pci总线（控制器）本身
/// 它对应/sys/device/pci
#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct PciBusDevice {
    inner: SpinLock<InnerPciBusDevice>,
    kobj_state: LockedKObjectState,
    name: String,
}

impl PciBusDevice {
    pub fn new(parent: Option<Weak<dyn KObject>>) -> Arc<Self> {
        let bus_device = Self {
            inner: SpinLock::new(InnerPciBusDevice {
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            kobj_state: LockedKObjectState::new(None),
            name: "pci".to_string(),
        };
        bus_device.set_parent(parent);
        return Arc::new(bus_device);
    }

    fn inner(&self) -> SpinLockGuard<InnerPciBusDevice> {
        self.inner.lock()
    }
}

#[derive(Debug)]
struct InnerPciBusDevice {
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
}

impl KObject for PciBusDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn kobj_state(
        &self,
    ) -> crate::libs::rwlock::RwLockReadGuard<crate::driver::base::kobject::KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn set_name(&self, _name: String) {
        //do nothing; it's not supposed to change this struct's name
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }
}

impl Device for PciBusDevice {
    fn dev_type(&self) -> DeviceType {
        return DeviceType::Bus;
    }

    fn id_table(&self) -> IdTable {
        IdTable::new("pci".to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner().device_common.driver.clone()?.upgrade()
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, driver: Option<alloc::sync::Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn can_match(&self) -> bool {
        todo!()
    }

    fn set_can_match(&self, _can_match: bool) {
        todo!()
    }

    fn set_class(&self, _class: Option<alloc::sync::Weak<dyn crate::driver::base::class::Class>>) {
        todo!()
    }

    fn state_synced(&self) -> bool {
        todo!()
    }

    fn dev_parent(&self) -> Option<alloc::sync::Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<alloc::sync::Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}
