use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::base::{
        class::Class,
        device::{
            bus::{Bus, BusState},
            device_manager,
            driver::Driver,
            Device, DevicePrivateData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::kernfs::KernFSInode,
    libs::{rwlock::RwLockWriteGuard, spinlock::SpinLock},
};

use super::{dev_id::PciDeviceID, pci_bus, pci_bus_device};

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
        if pci_dev.parent().is_none() {
            pci_dev.set_parent(Some(Arc::downgrade(
                &(pci_bus_device() as Arc<dyn KObject>),
            )));
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
}

/// #结构功能
/// 由于Pci总线本身就属于一个设备，故该结构代表Pci总线（控制器）本身
/// 它对应/sys/device/pci
#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct PciBusDevice {
    inner: SpinLock<InnerPciBusDevice>,
    kobj_state: LockedKObjectState,
}


#[allow(dead_code)]
#[derive(Debug)]
pub struct InnerPciBusDevice {
    name: String,
    data: DevicePrivateData,
    state: BusState,
    parent: Option<Weak<dyn KObject>>,

    kernfs_inode: Option<Arc<KernFSInode>>,

    bus: Option<Weak<dyn Bus>>,
    driver: Option<Weak<dyn Driver>>,

    ktype: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
}

impl InnerPciBusDevice {
    pub fn new(data: DevicePrivateData, parent: Option<Weak<dyn KObject>>) -> Self {
        Self {
            data,
            name: "pci".to_string(),
            state: BusState::NotInitialized,
            parent,
            kernfs_inode: None,
            bus: None,
            driver: None,
            ktype: None,
            kset: None,
        }
    }
}

impl PciBusDevice {
    pub fn new(data: DevicePrivateData, parent: Option<Weak<dyn KObject>>) -> Arc<Self> {
        return Arc::new(Self {
            inner: SpinLock::new(InnerPciBusDevice::new(data, parent)),
            kobj_state: LockedKObjectState::new(None),
        });
    }
}

impl KObject for PciBusDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner.lock().parent.clone()
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kernfs_inode.clone()
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kernfs_inode = inode;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.lock().ktype
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().ktype = ktype
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.lock().kset.clone()
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
        self.inner.lock().name.clone()
    }

    fn set_name(&self, name: String) {
        self.inner.lock().name = name;
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.lock().kset = kset;
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.lock().parent = parent;
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
        self.inner.lock().bus.clone()
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn Bus>>) {
        self.inner.lock().bus = bus
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.lock().driver.clone()?.upgrade()
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn set_driver(&self, driver: Option<alloc::sync::Weak<dyn Driver>>) {
        self.inner.lock().driver = driver;
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
}


/// #结构功能
/// 由于每个PciDevice都需要一些共有的结构，所以这里将其抽象出来作为一个结构
#[derive(Debug)]
pub struct InnerPciDevice {
    /// 设备所在的总线
    bus: Option<Weak<dyn Bus>>,
    /// 设备的名称
    pub name: Option<String>,
    /// 设备的类
    pub class: Option<Weak<dyn Class>>,
    /// 设备绑定的驱动
    pub driver: Option<Weak<dyn Driver>>,
    pub kern_inode: Option<Arc<KernFSInode>>,
    pub parent: Option<Weak<dyn KObject>>,
    pub kset: Option<Arc<KSet>>,
    pub kobj_type: Option<&'static dyn KObjType>,
}

impl InnerPciDevice {
    pub fn default() -> Self {
        Self {
            bus: None,
            class: None,
            name: None,
            driver: None,
            kern_inode: None,
            parent: None,
            kset: None,
            kobj_type: None,
        }
    }

    pub fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.bus.clone()
    }

    pub fn set_bus(&mut self, bus: Option<Weak<dyn Bus>>) {
        self.bus = bus
    }

    pub fn set_class(&mut self, class: Option<Weak<dyn Class>>) {
        self.class = class
    }

    pub fn set_driver(&mut self, driver: Option<Weak<dyn Driver>>) {
        self.driver = driver
    }
}
