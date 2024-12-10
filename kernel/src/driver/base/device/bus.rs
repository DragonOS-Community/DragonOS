use super::{
    driver::{Driver, DriverMatchName, DriverMatcher},
    sys_devices_kset, Device, DeviceMatchName, DeviceMatcher, DeviceState,
};
use crate::{
    driver::base::{
        device::{device_manager, driver::driver_manager},
        kobject::{KObjType, KObject, KObjectManager},
        kset::KSet,
        subsys::SubSysPrivate,
    },
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, sysfs_instance, Attribute, AttributeGroup, SysFSOps,
            SysFSOpsSupport, SYSFS_ATTR_MODE_RW, SYSFS_ATTR_MODE_WO,
        },
        vfs::syscall::ModeType,
    },
    libs::rwlock::RwLock,
};
use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use core::{ffi::CStr, fmt::Debug, intrinsics::unlikely};
use hashbrown::HashMap;
use intertrait::cast::CastArc;
use log::{debug, error, info};
use system_error::SystemError;

/// `/sys/bus`的kset
static mut BUS_KSET_INSTANCE: Option<Arc<KSet>> = None;
/// `/sys/devices/system`的kset
static mut DEVICES_SYSTEM_KSET_INSTANCE: Option<Arc<KSet>> = None;

static mut BUS_MANAGER_INSTANCE: Option<BusManager> = None;

#[inline(always)]
pub fn sys_bus_kset() -> Arc<KSet> {
    unsafe { BUS_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
#[allow(dead_code)]
pub fn sys_devices_system_kset() -> Arc<KSet> {
    unsafe { DEVICES_SYSTEM_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
pub fn bus_manager() -> &'static BusManager {
    unsafe { BUS_MANAGER_INSTANCE.as_ref().unwrap() }
}

#[inline(always)]
pub fn subsystem_manager() -> &'static SubSystemManager {
    &SubSystemManager
}

/// @brief: 总线状态
#[derive(Debug, Copy, Clone)]
pub enum BusState {
    NotInitialized = 0, // 未初始化
    Initialized = 1,    // 已初始化
    UnDefined = 2,      // 未定义的
}

/// @brief: 将u32类型转换为总线状态类型
impl From<u32> for BusState {
    fn from(state: u32) -> Self {
        match state {
            0 => BusState::NotInitialized,
            1 => BusState::Initialized,
            _ => BusState::UnDefined,
        }
    }
}

/// @brief: 将总线状态类型转换为u32类型
impl From<DeviceState> for BusState {
    fn from(state: DeviceState) -> Self {
        match state {
            DeviceState::Initialized => BusState::Initialized,
            DeviceState::NotInitialized => BusState::NotInitialized,
            DeviceState::UnDefined => BusState::UnDefined,
        }
    }
}

/// @brief: 将总线状态类型转换为设备状态类型
impl From<BusState> for DeviceState {
    fn from(state: BusState) -> Self {
        match state {
            BusState::Initialized => DeviceState::Initialized,
            BusState::NotInitialized => DeviceState::NotInitialized,
            BusState::UnDefined => DeviceState::UnDefined,
        }
    }
}

/// 总线子系统的trait，所有总线都应实现该trait
///
/// 请注意，这个trait是用于实现总线子系统的，而不是总线驱动/总线设备。
/// https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/device/bus.h#84
pub trait Bus: Debug + Send + Sync {
    fn name(&self) -> String;
    /// Used for subsystems to enumerate devices like ("foo%u", dev->id).
    fn dev_name(&self) -> String;
    fn root_device(&self) -> Option<Weak<dyn Device>> {
        None
    }

    fn set_root_device(&self, _dev: Option<Weak<dyn Device>>) {}

    /// 总线上的设备的默认属性组
    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        &[]
    }

    /// 总线的默认属性组
    fn bus_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        &[]
    }

    /// 总线上的驱动的默认属性组
    fn drv_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        &[]
    }

    /// 检查设备是否可以被总线绑定，如果可以，就绑定它们。
    /// 绑定之后,device的driver字段会被设置为驱动实例。
    ///
    /// ## 参数
    ///
    /// - `device` - 设备实例
    ///
    /// ## 默认实现
    ///
    /// 如果总线不支持该操作，返回`SystemError::ENOSYS`
    fn probe(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        return Err(SystemError::ENOSYS);
    }
    fn remove(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError>;
    fn sync_state(&self, _device: &Arc<dyn Device>) {}
    fn shutdown(&self, _device: &Arc<dyn Device>);
    fn suspend(&self, _device: &Arc<dyn Device>) {
        // todo: implement suspend
    }

    fn resume(&self, device: &Arc<dyn Device>) -> Result<(), SystemError>;

    /// match device to driver.
    ///
    /// ## 参数
    ///
    /// * `device` - device
    /// * `driver` - driver
    ///
    /// ## 返回
    ///
    /// - `Ok(true)` - 匹配成功
    /// - `Ok(false)` - 匹配失败
    /// - `Err(_)` - 由于内部错误导致匹配失败
    /// - `Err(SystemError::ENOSYS)` - 该总线不支持该操作
    fn match_device(
        &self,
        _device: &Arc<dyn Device>,
        _driver: &Arc<dyn Driver>,
    ) -> Result<bool, SystemError> {
        return Err(SystemError::ENOSYS);
    }

    fn subsystem(&self) -> &SubSysPrivate;

    /// 对当前总线操作的时候需要获取父级总线的锁
    fn need_parent_lock(&self) -> bool {
        false
    }
}

impl dyn Bus {
    /// 在bus上,根据条件寻找一个特定的设备
    ///
    /// ## 参数
    ///
    /// - `matcher` - 匹配器
    /// - `data` - 传给匹配器的数据
    pub fn find_device<T: Copy>(
        &self,
        matcher: &dyn DeviceMatcher<T>,
        data: T,
    ) -> Option<Arc<dyn Device>> {
        let subsys = self.subsystem();
        let guard = subsys.devices();
        for dev in guard.iter() {
            if matcher.match_device(dev, data) {
                return Some(dev.clone());
            }
        }
        return None;
    }

    /// 根据名称匹配设备
    ///
    /// ## 参数
    ///
    /// - name 设备名称
    pub fn find_device_by_name(&self, name: &str) -> Option<Arc<dyn Device>> {
        return self.find_device(&DeviceMatchName, name);
    }

    /// 在bus上,根据条件寻找一个特定的驱动
    ///
    /// ## 参数
    ///
    /// - `matcher` - 匹配器
    /// - `data` - 传给匹配器的数据
    pub fn find_driver<T: Copy>(
        &self,
        matcher: &dyn DriverMatcher<T>,
        data: T,
    ) -> Option<Arc<dyn Driver>> {
        let subsys = self.subsystem();
        let guard = subsys.drivers();
        for drv in guard.iter() {
            if matcher.match_driver(drv, data) {
                return Some(drv.clone());
            }
        }
        return None;
    }

    /// 根据名称在bus上匹配驱动
    pub fn find_driver_by_name(&self, name: &str) -> Option<Arc<dyn Driver>> {
        return self.find_driver(&DriverMatchName, name);
    }
}

/// @brief: 总线管理结构体
#[derive(Debug)]
pub struct BusManager {
    /// 存储总线bus的kset结构体与bus实例的映射(用于在sysfs callback的时候,根据kset找到bus实例)
    kset_bus_map: RwLock<HashMap<Arc<KSet>, Arc<dyn Bus>>>,
}

impl BusManager {
    pub fn new() -> Self {
        return Self {
            kset_bus_map: RwLock::new(HashMap::new()),
        };
    }

    /// 把一个设备添加到总线上
    ///
    /// ## 描述
    ///
    /// - 添加一个设备的与bus相关的属性
    /// - 在bus和设备文件夹下，创建软链接
    /// - 把设备添加到它的总线的设备列表中
    ///
    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_add_device#441
    ///
    /// ## 参数
    ///
    /// - `dev` - 要被添加的设备
    pub fn add_device(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let bus = dev.bus().and_then(|bus| bus.upgrade());
        if let Some(bus) = bus {
            device_manager().add_groups(dev, bus.dev_groups())?;

            // 增加符号链接
            let bus_devices_kset = bus
                .subsystem()
                .devices_kset()
                .expect("bus devices kset is none, maybe bus is not registered");
            let dev_kobj = dev.clone() as Arc<dyn KObject>;

            sysfs_instance().create_link(
                Some(&bus_devices_kset.as_kobject()),
                &dev_kobj,
                dev.name(),
            )?;
            sysfs_instance().create_link(
                Some(&dev_kobj),
                &bus.subsystem().subsys().as_kobject(),
                "subsystem".to_string(),
            )?;
            bus.subsystem().add_device_to_vec(dev)?;
        }
        return Ok(());
    }

    /// 在总线上添加一个驱动
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_add_driver#590
    pub fn add_driver(&self, driver: &Arc<dyn Driver>) -> Result<(), SystemError> {
        let bus = driver
            .bus()
            .and_then(|bus| bus.upgrade())
            .ok_or(SystemError::EINVAL)?;
        debug!("bus '{}' add driver '{}'", bus.name(), driver.name());

        // driver.set_kobj_type(Some(&BusDriverKType));
        let kobj = driver.clone() as Arc<dyn KObject>;
        // KObjectManager::add_kobj(kobj, bus.subsystem().drivers_kset())?;
        KObjectManager::init_and_add_kobj(
            kobj,
            bus.subsystem().drivers_kset(),
            Some(&BusDriverKType),
        )?;

        bus.subsystem().add_driver_to_vec(driver)?;
        if bus.subsystem().drivers_autoprobe() {
            let r = driver_manager().driver_attach(driver);
            if let Err(e) = r {
                bus.subsystem().remove_driver_from_vec(driver);
                return Err(e);
            }
        }

        driver_manager()
            .add_groups(driver, bus.drv_groups())
            .map_err(|e| {
                error!(
                    "BusManager::add_driver: driver '{:?}' add_groups failed, err: '{:?}",
                    driver.name(),
                    e
                );
                e
            })
            .ok();

        if !driver.suppress_bind_attrs() {
            self.add_bind_files(driver)
                .map_err(|e| {
                    error!(
                        "BusManager::add_driver: driver '{:?}' add_bind_files failed, err: '{:?}",
                        driver.name(),
                        e
                    );
                    e
                })
                .ok();
        }

        return Ok(());
    }

    ///
    /// bus_register - register a driver-core subsystem
    ///
    /// ## 参数
    /// - `bus` - bus to register
    ///
    /// Once we have that, we register the bus with the kobject
    /// infrastructure, then register the children subsystems it has:
    /// the devices and drivers that belong to the subsystem.
    ///
    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_register#783
    ///
    /// todo: 增加错误处理逻辑
    pub fn register(&self, bus: Arc<dyn Bus>) -> Result<(), SystemError> {
        bus.subsystem().set_bus(Some(Arc::downgrade(&bus)));

        let subsys_kset = bus.subsystem().subsys();
        subsys_kset.set_name(bus.name());
        bus.subsystem().set_drivers_autoprobe(true);

        subsys_kset.register(Some(sys_bus_kset()))?;

        let devices_kset =
            KSet::new_and_add("devices".to_string(), None, Some(subsys_kset.clone()))?;
        bus.subsystem().set_devices_kset(devices_kset);
        let drivers_kset =
            KSet::new_and_add("drivers".to_string(), None, Some(subsys_kset.clone()))?;
        bus.subsystem().set_drivers_kset(drivers_kset);

        self.add_probe_files(&bus)?;
        let bus_groups = bus.bus_groups();
        self.add_groups(&bus, bus_groups)?;
        // 把bus实例添加到总线管理器中（方便在sysfs callback的时候,根据kset找到bus实例）
        self.kset_bus_map.write().insert(subsys_kset, bus.clone());
        return Ok(());
    }

    pub fn unregister(&self, _bus: Arc<dyn Bus>) -> Result<(), SystemError> {
        todo!("bus_unregister")
    }

    fn add_probe_files(&self, bus: &Arc<dyn Bus>) -> Result<(), SystemError> {
        self.create_file(bus, &BusAttrDriversProbe)?;
        let r = self.create_file(bus, &BusAttrDriversAutoprobe);

        if r.is_err() {
            self.remove_file(bus, &BusAttrDriversProbe);
        }
        return r;
    }

    #[allow(dead_code)]
    fn remove_probe_files(&self, bus: &Arc<dyn Bus>) {
        self.remove_file(bus, &BusAttrDriversAutoprobe);
        self.remove_file(bus, &BusAttrDriversProbe);
    }

    fn create_file(
        &self,
        bus: &Arc<dyn Bus>,
        attr: &'static dyn Attribute,
    ) -> Result<(), SystemError> {
        let bus_kobj = bus.subsystem().subsys() as Arc<dyn KObject>;
        return sysfs_instance().create_file(&bus_kobj, attr);
    }

    fn remove_file(&self, bus: &Arc<dyn Bus>, attr: &'static dyn Attribute) {
        let bus_kobj = bus.subsystem().subsys() as Arc<dyn KObject>;
        sysfs_instance().remove_file(&bus_kobj, attr);
    }

    #[inline]
    fn add_groups(
        &self,
        bus: &Arc<dyn Bus>,
        groups: &[&'static dyn AttributeGroup],
    ) -> Result<(), SystemError> {
        let bus_kobj = bus.subsystem().subsys() as Arc<dyn KObject>;
        return sysfs_instance().create_groups(&bus_kobj, groups);
    }

    /// 根据bus的kset找到bus实例
    fn get_bus_by_kset(&self, kset: &Arc<KSet>) -> Option<Arc<dyn Bus>> {
        return self.kset_bus_map.read().get(kset).cloned();
    }

    /// 为bus上的设备选择可能的驱动程序
    ///
    /// 这个函数会扫描总线上的所有没有驱动的设备，然后为它们选择可能的驱动程序。
    ///
    /// ## 参数
    ///
    /// - `bus` - bus实例
    #[allow(dead_code)]
    pub fn rescan_devices(&self, bus: &Arc<dyn Bus>) -> Result<(), SystemError> {
        for dev in bus.subsystem().devices().iter() {
            rescan_devices_helper(dev)?;
        }
        return Ok(());
    }

    /// 为新设备探测驱动
    ///
    /// Automatically probe for a driver if the bus allows it.
    pub fn probe_device(&self, dev: &Arc<dyn Device>) {
        let bus = dev.bus().and_then(|bus| bus.upgrade());
        if bus.is_none() {
            return;
        }
        let bus = bus.unwrap();
        if bus.subsystem().drivers_autoprobe() {
            log::info!("MT bus '{}' autoprobe driver", bus.name());
            device_manager().device_initial_probe(dev).ok();
        }
        for interface in bus.subsystem().interfaces() {
            interface.add_device(dev).ok();
        }
    }

    /// 从总线上移除一个驱动
    ///
    /// Detach the driver from the devices it controls, and remove
    /// it from its bus's list of drivers. Finally, we drop the reference
    /// to the bus.
    ///
    /// ## 参数
    ///
    /// - `driver` - 驱动实例
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_remove_driver#666
    pub fn remove_driver(&self, _driver: &Arc<dyn Driver>) {
        todo!("BusManager::remove_driver")
    }

    fn add_bind_files(&self, driver: &Arc<dyn Driver>) -> Result<(), SystemError> {
        driver_manager().create_attr_file(driver, &DriverAttrUnbind)?;

        driver_manager()
            .create_attr_file(driver, &DriverAttrBind)
            .inspect_err(|_e| {
                driver_manager().remove_attr_file(driver, &DriverAttrUnbind);
            })?;

        return Ok(());
    }
}

/// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#684
fn rescan_devices_helper(dev: &Arc<dyn Device>) -> Result<(), SystemError> {
    if dev.driver().is_none() {
        let need_parent_lock = dev
            .bus()
            .map(|bus| bus.upgrade().unwrap().need_parent_lock())
            .unwrap_or(false);
        if unlikely(need_parent_lock) {
            // todo: lock device parent
            unimplemented!()
        }
        device_manager().device_attach(dev)?;
    }
    return Ok(());
}

///
/// bus_register - register a driver-core subsystem
///
/// ## 参数
/// - `bus` - bus to register
///
/// Once we have that, we register the bus with the kobject
/// infrastructure, then register the children subsystems it has:
/// the devices and drivers that belong to the subsystem.
pub fn bus_register(bus: Arc<dyn Bus>) -> Result<(), SystemError> {
    return bus_manager().register(bus);
}

/// @brief: 总线注销，并在sys/bus和sys/devices下删除文件夹
/// @parameter bus: Bus设备实体
/// @return: 成功:()   失败:SystemError
#[allow(dead_code)]
pub fn bus_unregister(bus: Arc<dyn Bus>) -> Result<(), SystemError> {
    return bus_manager().unregister(bus);
}

pub fn buses_init() -> Result<(), SystemError> {
    let bus_kset = KSet::new("bus".to_string());
    bus_kset.register(None).expect("bus kset register failed");
    unsafe {
        BUS_KSET_INSTANCE = Some(bus_kset);
    }

    // 初始化 /sys/devices/system
    {
        let devices_system_kset = KSet::new("system".to_string());
        let parent = sys_devices_kset() as Arc<dyn KObject>;
        devices_system_kset.set_parent(Some(Arc::downgrade(&parent)));
        devices_system_kset
            .register(Some(sys_devices_kset()))
            .expect("devices system kset register failed");
        unsafe {
            DEVICES_SYSTEM_KSET_INSTANCE = Some(devices_system_kset);
        }
    }

    // 初始化总线管理器
    {
        let bus_manager = BusManager::new();
        unsafe {
            BUS_MANAGER_INSTANCE = Some(bus_manager);
        }
    }
    return Ok(());
}

/// 把一个设备添加到总线上
///
/// ## 描述
///
/// - 添加一个设备的与bus相关的属性
/// - 在bus和设备文件夹下，创建软链接
/// - 把设备添加到它的总线的设备列表中
///
/// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_add_device#441
///
/// ## 参数
///
/// - `dev` - 要被添加的设备
pub fn bus_add_device(dev: &Arc<dyn Device>) -> Result<(), SystemError> {
    return bus_manager().add_device(dev);
}

/// 自动为设备在总线上寻找可用的驱动程序
///
/// Automatically probe for a driver if the bus allows it.
///
/// ## 参数
///
/// - `dev` - 要被添加的设备
///
/// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_probe_device#478
pub fn bus_probe_device(dev: &Arc<dyn Device>) {
    info!("bus_probe_device: dev: {:?}", dev.name());
    bus_manager().probe_device(dev);
}

#[derive(Debug)]
struct BusAttrDriversProbe;

impl Attribute for BusAttrDriversProbe {
    fn mode(&self) -> ModeType {
        return ModeType::S_IWUSR;
    }

    fn name(&self) -> &str {
        return "drivers_probe";
    }

    fn support(&self) -> SysFSOpsSupport {
        return SysFSOpsSupport::ATTR_STORE;
    }

    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#241
    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let kset: Arc<KSet> = kobj.arc_any().downcast().map_err(|_| SystemError::EINVAL)?;
        let bus = bus_manager()
            .get_bus_by_kset(&kset)
            .ok_or(SystemError::EINVAL)?;

        let name = CStr::from_bytes_with_nul(buf)
            .map_err(|_| SystemError::EINVAL)?
            .to_str()
            .map_err(|_| SystemError::EINVAL)?;

        let device = bus.find_device_by_name(name).ok_or(SystemError::ENODEV)?;

        if rescan_devices_helper(&device).is_ok() {
            return Ok(buf.len());
        }

        return Err(SystemError::EINVAL);
    }
}

#[derive(Debug)]
struct BusAttrDriversAutoprobe;

impl Attribute for BusAttrDriversAutoprobe {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_RW
    }

    fn name(&self) -> &str {
        return "drivers_autoprobe";
    }

    fn support(&self) -> SysFSOpsSupport {
        return SysFSOpsSupport::ATTR_STORE | SysFSOpsSupport::ATTR_SHOW;
    }

    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#231
    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        if buf.is_empty() {
            return Ok(0);
        }

        let kset: Arc<KSet> = kobj.arc_any().downcast().map_err(|_| SystemError::EINVAL)?;
        let bus = bus_manager()
            .get_bus_by_kset(&kset)
            .ok_or(SystemError::EINVAL)?;

        if buf[0] == b'0' {
            bus.subsystem().set_drivers_autoprobe(false);
        } else {
            bus.subsystem().set_drivers_autoprobe(true);
        }

        return Ok(buf.len());
    }

    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#226
    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let kset: Arc<KSet> = kobj.arc_any().downcast().map_err(|_| SystemError::EINVAL)?;
        let bus = bus_manager()
            .get_bus_by_kset(&kset)
            .ok_or(SystemError::EINVAL)?;
        let val = if bus.subsystem().drivers_autoprobe() {
            1
        } else {
            0
        };
        return sysfs_emit_str(buf, format!("{val}\n").as_str());
    }
}

#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub enum BusNotifyEvent {
    /// 一个设备被添加到总线上
    AddDevice,
    /// 一个设备将要被移除
    DelDevice,
    /// 一个设备已经被移除
    RemovedDevice,
    /// 一个驱动将要被绑定
    BindDriver,
    /// 一个驱动已经被绑定
    BoundDriver,
    /// 一个驱动将要被解绑
    UnbindDriver,
    /// 一个驱动已经被解绑
    UnboundDriver,
    /// 驱动绑定失败
    DriverNotBound,
}

#[derive(Debug)]
struct BusDriverKType;

impl KObjType for BusDriverKType {
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&BusDriverSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}

#[derive(Debug)]
struct BusDriverSysFSOps;

impl SysFSOps for BusDriverSysFSOps {
    #[inline]
    fn show(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        attr.show(kobj, buf)
    }

    #[inline]
    fn store(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        attr.store(kobj, buf)
    }
}

#[derive(Debug)]
struct DriverAttrUnbind;

impl Attribute for DriverAttrUnbind {
    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_WO
    }

    fn name(&self) -> &str {
        "unbind"
    }

    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let driver = kobj.cast::<dyn Driver>().map_err(|kobj| {
            error!(
                "Intertrait casting not implemented for kobj: {}",
                kobj.name()
            );
            SystemError::ENOSYS
        })?;

        let bus = driver
            .bus()
            .and_then(|bus| bus.upgrade())
            .ok_or(SystemError::ENODEV)?;

        let s = CStr::from_bytes_with_nul(buf)
            .map_err(|_| SystemError::EINVAL)?
            .to_str()
            .map_err(|_| SystemError::EINVAL)?;
        let dev = bus.find_device_by_name(s).ok_or(SystemError::ENODEV)?;
        let p = dev.driver().ok_or(SystemError::ENODEV)?;
        if Arc::ptr_eq(&p, &driver) {
            device_manager().device_driver_detach(&dev);
            return Ok(buf.len());
        }
        return Err(SystemError::ENODEV);
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_STORE
    }
}

#[derive(Debug)]
struct DriverAttrBind;

impl Attribute for DriverAttrBind {
    fn name(&self) -> &str {
        "bind"
    }

    fn mode(&self) -> ModeType {
        SYSFS_ATTR_MODE_WO
    }

    /*
     * Manually attach a device to a driver.
     * Note: the driver must want to bind to the device,
     * it is not possible to override the driver's id table.
     */
    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        let driver = kobj.cast::<dyn Driver>().map_err(|kobj| {
            error!(
                "Intertrait casting not implemented for kobj: {}",
                kobj.name()
            );
            SystemError::ENOSYS
        })?;

        let bus = driver
            .bus()
            .and_then(|bus| bus.upgrade())
            .ok_or(SystemError::ENODEV)?;
        let device = bus
            .find_device_by_name(
                CStr::from_bytes_with_nul(buf)
                    .map_err(|_| SystemError::EINVAL)?
                    .to_str()
                    .map_err(|_| SystemError::EINVAL)?,
            )
            .ok_or(SystemError::ENODEV)?;

        if driver_manager().match_device(&driver, &device)? {
            device_manager().device_driver_attach(&driver, &device)?;
            return Ok(buf.len());
        }
        return Err(SystemError::ENODEV);
    }
    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_STORE
    }
}

#[derive(Debug)]
pub struct SubSystemManager;

impl SubSystemManager {
    /// 注册一个子系统，并在`/sys/bus`和指定的父级文件夹下创建子文件夹
    ///
    /// ## 参数
    ///
    /// - `subsys` - 子系统实例
    /// - `fake_root_dev` - 该子系统的伪根设备
    /// - `parent_of_root` - 该子系统的伪根设备的父级节点
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=subsys_system_register#1078
    pub fn subsys_register(
        &self,
        subsys: &Arc<dyn Bus>,
        fake_root_dev: &Arc<dyn Device>,
        parent_of_root: &Arc<dyn KObject>,
    ) -> Result<(), SystemError> {
        bus_manager().register(subsys.clone())?;
        fake_root_dev.set_name(subsys.name());
        fake_root_dev.set_parent(Some(Arc::downgrade(parent_of_root)));

        device_manager().register(fake_root_dev.clone())?;

        subsys.set_root_device(Some(Arc::downgrade(fake_root_dev)));
        return Ok(());
    }

    /// register a subsystem at /sys/devices/system/
    /// 并且在/sys/bus和/sys/devices下创建文件夹
    ///
    /// All 'system' subsystems have a /sys/devices/system/<name> root device
    /// with the name of the subsystem. The root device can carry subsystem-
    /// wide attributes. All registered devices are below this single root
    /// device and are named after the subsystem with a simple enumeration
    /// number appended. The registered devices are not explicitly named;
    /// only 'id' in the device needs to be set.
    pub fn subsys_system_register(
        &self,
        subsys: &Arc<dyn Bus>,
        fake_root_dev: &Arc<dyn Device>,
    ) -> Result<(), SystemError> {
        return self.subsys_register(
            subsys,
            fake_root_dev,
            &(sys_devices_system_kset() as Arc<dyn KObject>),
        );
    }
}
