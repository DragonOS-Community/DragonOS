use super::{
    device_register, device_unregister,
    driver::{driver_register, driver_unregister, DriverError},
    sys_devices_kset, Device, DeviceError, DeviceMatchName, DeviceMatcher, DeviceState, IdTable,
};
use crate::{
    driver::{
        base::{
            device::{device_manager, DeviceManager},
            kobject::KObject,
            kset::KSet,
            SubSysPrivate,
        },
        Driver,
    },
    filesystem::{
        sysfs::{
            bus::{sys_bus_init, sys_bus_register},
            sysfs_instance, Attribute, AttributeGroup, SysFSOpsSupport, SYS_BUS_INODE,
        },
        vfs::{syscall::ModeType, IndexNode},
    },
    libs::{rwlock::RwLock, spinlock::SpinLock},
    syscall::SystemError,
};
use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use core::{ffi::CStr, fmt::Debug, intrinsics::unlikely};
use hashbrown::HashMap;

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
pub fn sys_devices_system_kset() -> Arc<KSet> {
    unsafe { DEVICES_SYSTEM_KSET_INSTANCE.clone().unwrap() }
}

#[inline(always)]
pub fn bus_manager() -> &'static BusManager {
    unsafe { BUS_MANAGER_INSTANCE.as_ref().unwrap() }
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

/// @brief: 总线驱动trait，所有总线驱动都应实现该trait
pub trait BusDriver: Driver {
    /// @brief: 判断总线是否为空
    /// @parameter: None
    /// @return: 如果总线上设备和驱动的数量都为0，则返回true，否则，返回false
    fn is_empty(&self) -> bool;
}

/// 总线子系统的trait，所有总线都应实现该trait
///
/// 请注意，这个trait是用于实现总线子系统的，而不是总线驱动/总线设备。
/// https://opengrok.ringotek.cn/xref/linux-6.1.9/include/linux/device/bus.h#84
pub trait Bus: Debug + Send + Sync {
    fn name(&self) -> String;
    fn dev_name(&self) -> String;
    fn root_device(&self) -> Option<Arc<dyn Device>> {
        None
    }

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
    pub fn find_device<T:Copy>(
        &self,
        matcher: &dyn DeviceMatcher<T>,
        data: T,
    ) -> Option<Arc<dyn Device>> {
        let subsys = self.subsystem();
        let guard = subsys.devices.read();
        for dev in guard.iter() {
            let dev = dev.upgrade();
            if let Some(dev) = dev {
                if matcher.match_device(&dev, data) {
                    return Some(dev.clone());
                }
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
}

/// @brief: 总线管理结构体
#[derive(Debug)]
pub struct BusManager {
    /// 存储总线bus的kset结构体与bus实例的映射(用于在sysfs callback的时候,根据kset找到bus实例)
    kset_bus_map: RwLock<HashMap<Arc<KSet>, Arc<dyn Bus>>>,
}

impl BusManager {
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
    /// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_register#783
    pub fn register(&self, bus: Arc<dyn Bus>) -> Result<(), SystemError> {
        bus.subsystem().set_bus(Arc::downgrade(&bus));

        let subsys_kset = bus.subsystem().subsys();
        subsys_kset.set_name(bus.name());
        bus.subsystem().set_drivers_autoprobe(true);

        subsys_kset.register(Some(sys_bus_kset()));

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

    pub fn unregister(&self, bus: Arc<dyn Bus>) -> Result<(), SystemError> {
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

    fn remove_probe_files(&self, bus: &Arc<dyn Bus>) {
        self.remove_file(bus, &BusAttrDriversAutoprobe);
        self.remove_file(bus, &BusAttrDriversProbe);
    }

    fn create_file(&self, bus: &Arc<dyn Bus>, attr: &'static dyn Attribute) -> Result<(), SystemError> {
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
        return self.kset_bus_map.read().get(kset).map(|bus| bus.clone());
    }

    /// 为bus上的设备选择可能的驱动程序
    ///
    /// 这个函数会扫描总线上的所有没有驱动的设备，然后为它们选择可能的驱动程序。
    ///
    /// ## 参数
    ///
    /// - `bus` - bus实例
    pub fn rescan_devices(&self, bus: &Arc<dyn Bus>) -> Result<(), SystemError> {
        for dev in bus.subsystem().devices.read().iter() {
            let dev = dev.upgrade();
            if let Some(dev) = dev {
                rescan_devices_helper(dev)?;
            }
        }
        return Ok(());
    }
}

/// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#684
fn rescan_devices_helper(dev: Arc<dyn Device>) -> Result<(), SystemError> {
    if dev.driver().is_none() {
        let need_parent_lock = dev.bus().map(|bus| bus.need_parent_lock()).unwrap_or(false);
        if unlikely(need_parent_lock) {
            // todo: lock device parent
            unimplemented!()
        }
        device_manager().device_attach(&dev)?;
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

/// @brief: 总线驱动注册，将总线驱动加入全局总线管理器中
/// @parameter bus: Bus设备驱动实体
/// @return: 成功:()   失败:DeviceError
pub fn bus_driver_register(bus_driver: Arc<dyn BusDriver>) -> Result<(), DriverError> {
    todo!("bus_driver_register")
}

/// @brief: 总线驱动注销，将总线从全局总线管理器中删除
/// @parameter bus: Bus设备驱动实体
/// @return: 成功:()   失败:DeviceError
#[allow(dead_code)]
pub fn bus_driver_unregister(bus_driver: Arc<dyn BusDriver>) -> Result<(), DriverError> {
    todo!("bus_driver_unregister")
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
/// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_add_device#441
///
/// ## 参数
///
/// - `dev` - 要被添加的设备
pub fn bus_add_device(dev: &Arc<dyn Device>) -> Result<(), SystemError> {
    let bus = dev.bus();
    if let Some(bus) = bus {
        device_manager().add_groups(dev, bus.dev_groups());
        // todo: 增加符号链接
        todo!("bus_add_device")
    }
    return Ok(());
}

/// 自动为设备在总线上寻找可用的驱动程序
///
/// Automatically probe for a driver if the bus allows it.
///
/// ## 参数
///
/// - `dev` - 要被添加的设备
///
/// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?fi=bus_probe_device#478
pub fn bus_probe_device(dev: &Arc<dyn Device>) {
    todo!("bus_probe_device")
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
        return SysFSOpsSupport::STORE;
    }

    /// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#241
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

        if rescan_devices_helper(device).is_ok() {
            return Ok(buf.len());
        }

        return Err(SystemError::EINVAL);
    }
}

#[derive(Debug)]
struct BusAttrDriversAutoprobe;

impl Attribute for BusAttrDriversAutoprobe {
    fn mode(&self) -> ModeType {
        return ModeType::from_bits_truncate(0o644);
    }

    fn name(&self) -> &str {
        return "drivers_autoprobe";
    }

    fn support(&self) -> SysFSOpsSupport {
        return SysFSOpsSupport::STORE | SysFSOpsSupport::SHOW;
    }

    /// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#231
    fn store(&self, kobj: Arc<dyn KObject>, buf: &[u8]) -> Result<usize, SystemError> {
        todo!("BusAttrDriversAutoprobe::store()")
    }

    /// 参考： https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/bus.c?r=&mo=5649&fi=241#226
    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!("BusAttrDriversAutoprobe::show()")
    }
}

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
