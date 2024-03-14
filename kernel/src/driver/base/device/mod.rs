use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use intertrait::cast::CastArc;

use crate::{
    driver::{
        acpi::glue::acpi_device_notify,
        base::map::{LockedDevsMap, LockedKObjMap},
    },
    exception::irqdata::IrqHandlerData,
    filesystem::{
        sysfs::{
            file::sysfs_emit_str, sysfs_instance, Attribute, AttributeGroup, SysFSOps,
            SysFSOpsSupport,
        },
        vfs::syscall::ModeType,
    },
};

use core::fmt::Debug;
use core::intrinsics::unlikely;
use system_error::SystemError;

use self::{
    bus::{bus_add_device, bus_probe_device, Bus},
    device_number::{DeviceNumber, Major},
    driver::Driver,
};

use super::{
    class::Class,
    kobject::{KObjType, KObject, KObjectManager, KObjectState},
    kset::KSet,
    swnode::software_node_notify,
};

pub mod bus;
pub mod dd;
pub mod device_number;
pub mod driver;
pub mod init;

static mut DEVICE_MANAGER: Option<DeviceManager> = None;

#[inline(always)]
pub fn device_manager() -> &'static DeviceManager {
    unsafe { DEVICE_MANAGER.as_ref().unwrap() }
}

lazy_static! {
    // 全局字符设备号管理实例
    pub static ref CHARDEVS: Arc<LockedDevsMap> = Arc::new(LockedDevsMap::default());

    // 全局块设备管理实例
    pub static ref BLOCKDEVS: Arc<LockedDevsMap> = Arc::new(LockedDevsMap::default());

    // 全局设备管理实例
    pub static ref DEVMAP: Arc<LockedKObjMap> = Arc::new(LockedKObjMap::default());

}

/// `/sys/devices` 的 kset 实例
static mut DEVICES_KSET_INSTANCE: Option<Arc<KSet>> = None;
/// `/sys/dev` 的 kset 实例
static mut DEV_KSET_INSTANCE: Option<Arc<KSet>> = None;
/// `/sys/dev/block` 的 kset 实例
static mut DEV_BLOCK_KSET_INSTANCE: Option<Arc<KSet>> = None;
/// `/sys/dev/char` 的 kset 实例
static mut DEV_CHAR_KSET_INSTANCE: Option<Arc<KSet>> = None;

/// `/sys/devices/virtual` 的 kset 实例
static mut DEVICES_VIRTUAL_KSET_INSTANCE: Option<Arc<KSet>> = None;

/// 获取`/sys/devices`的kset实例
#[inline(always)]
pub(super) fn sys_devices_kset() -> Arc<KSet> {
    unsafe { DEVICES_KSET_INSTANCE.as_ref().unwrap().clone() }
}

/// 获取`/sys/dev`的kset实例
#[inline(always)]
pub(super) fn sys_dev_kset() -> Arc<KSet> {
    unsafe { DEV_KSET_INSTANCE.as_ref().unwrap().clone() }
}

/// 获取`/sys/dev/block`的kset实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_dev_block_kset() -> Arc<KSet> {
    unsafe { DEV_BLOCK_KSET_INSTANCE.as_ref().unwrap().clone() }
}

/// 获取`/sys/dev/char`的kset实例
#[inline(always)]
pub fn sys_dev_char_kset() -> Arc<KSet> {
    unsafe { DEV_CHAR_KSET_INSTANCE.as_ref().unwrap().clone() }
}

unsafe fn set_sys_dev_block_kset(kset: Arc<KSet>) {
    DEV_BLOCK_KSET_INSTANCE = Some(kset);
}

unsafe fn set_sys_dev_char_kset(kset: Arc<KSet>) {
    DEV_CHAR_KSET_INSTANCE = Some(kset);
}

/// 获取`/sys/devices/virtual`的kset实例
pub fn sys_devices_virtual_kset() -> Arc<KSet> {
    unsafe { DEVICES_VIRTUAL_KSET_INSTANCE.as_ref().unwrap().clone() }
}

unsafe fn set_sys_devices_virtual_kset(kset: Arc<KSet>) {
    DEVICES_VIRTUAL_KSET_INSTANCE = Some(kset);
}

/// 设备应该实现的操作
///
/// ## 注意
///
/// 由于设备驱动模型需要从Arc<dyn KObject>转换为Arc<dyn Device>，
/// 因此，所有的实现了Device trait的结构体，都应该在结构体上方标注`#[cast_to([sync] Device)]`，
///
/// 否则在释放设备资源的时候，会由于无法转换为Arc<dyn Device>而导致资源泄露，并且release回调函数也不会被调用。
pub trait Device: KObject {
    // TODO: 待实现 open, close

    /// @brief: 获取设备类型
    /// @parameter: None
    /// @return: 实现该trait的设备所属类型
    fn dev_type(&self) -> DeviceType;

    /// @brief: 获取设备标识
    /// @parameter: None
    /// @return: 该设备唯一标识
    fn id_table(&self) -> IdTable;

    /// 设备释放时的回调函数
    fn release(&self) {
        let name = self.name();
        kwarn!(
            "device {} does not have a release() function, it is broken and must be fixed.",
            name
        );
    }

    /// 获取当前设备所属的总线
    fn bus(&self) -> Option<Weak<dyn Bus>> {
        return None;
    }

    /// 设置当前设备所属的总线
    ///
    /// （一定要传入Arc，因为bus的subsysprivate里面存储的是Device的Arc指针）
    ///
    /// 注意，如果实现了当前方法，那么必须实现`bus()`方法
    fn set_bus(&self, bus: Option<Weak<dyn Bus>>);

    /// 获取当前设备所属的类
    fn class(&self) -> Option<Arc<dyn Class>> {
        return None;
    }

    /// 设置当前设备所属的类
    ///
    /// 注意，如果实现了当前方法，那么必须实现`class()`方法
    fn set_class(&self, class: Option<Arc<dyn Class>>);

    /// 返回已经与当前设备匹配好的驱动程序
    fn driver(&self) -> Option<Arc<dyn Driver>>;

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>);

    /// 当前设备是否已经挂掉了
    fn is_dead(&self) -> bool;

    /// 当前设备是否处于可以被匹配的状态
    ///
    /// The device has matched with a driver at least once or it is in
    /// a bus (like AMBA) which can't check for matching drivers until
    /// other devices probe successfully.
    fn can_match(&self) -> bool;

    fn set_can_match(&self, can_match: bool);

    /// The hardware state of this device has been synced to match
    /// the software state of this device by calling the driver/bus
    /// sync_state() callback.
    fn state_synced(&self) -> bool;

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}

impl dyn Device {
    #[inline(always)]
    pub fn is_registered(&self) -> bool {
        self.kobj_state().contains(KObjectState::IN_SYSFS)
    }
}

// 暂定是不可修改的，在初始化的时候就要确定。以后可能会包括例如硬件中断包含的信息
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DevicePrivateData {
    id_table: IdTable,
    state: DeviceState,
}

#[allow(dead_code)]
impl DevicePrivateData {
    pub fn new(id_table: IdTable, state: DeviceState) -> Self {
        Self { id_table, state }
    }

    pub fn id_table(&self) -> &IdTable {
        &self.id_table
    }

    pub fn state(&self) -> DeviceState {
        self.state
    }

    pub fn set_state(&mut self, state: DeviceState) {
        self.state = state;
    }
}

/// @brief: 设备类型
#[allow(dead_code)]
#[derive(Debug, Eq, PartialEq)]
pub enum DeviceType {
    Bus,
    Net,
    Gpu,
    Input,
    Block,
    Rtc,
    Serial,
    Intc,
    PlatformDev,
    Char,
}

/// @brief: 设备标识符类型
#[derive(Debug, Clone, Hash, PartialOrd, PartialEq, Ord, Eq)]
pub struct IdTable {
    basename: String,
    id: Option<DeviceNumber>,
}

/// @brief: 设备标识符操作方法集
impl IdTable {
    /// @brief: 创建一个新的设备标识符
    /// @parameter name: 设备名
    /// @parameter id: 设备id
    /// @return: 设备标识符
    pub fn new(basename: String, id: Option<DeviceNumber>) -> IdTable {
        return IdTable { basename, id };
    }

    /// @brief: 将设备标识符转换成name
    /// @parameter None
    /// @return: 设备名
    pub fn name(&self) -> String {
        if self.id.is_none() {
            return self.basename.clone();
        } else {
            let id = self.id.unwrap();
            return format!("{}:{}", id.major().data(), id.minor());
        }
    }

    pub fn device_number(&self) -> DeviceNumber {
        return self.id.unwrap_or(DeviceNumber::default());
    }
}

impl Default for IdTable {
    fn default() -> Self {
        IdTable::new("unknown".to_string(), None)
    }
}

// 以现在的模型，设备在加载到系统中就是已经初始化的状态了，因此可以考虑把这个删掉
/// @brief: 设备当前状态
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum DeviceState {
    NotInitialized = 0,
    Initialized = 1,
    UnDefined = 2,
}

/// @brief: 设备错误类型
#[allow(dead_code)]
#[derive(Debug, Copy, Clone)]
pub enum DeviceError {
    DriverExists,         // 设备已存在
    DeviceExists,         // 驱动已存在
    InitializeFailed,     // 初始化错误
    NotInitialized,       // 未初始化的设备
    NoDeviceForDriver,    // 没有合适的设备匹配驱动
    NoDriverForDevice,    // 没有合适的驱动匹配设备
    RegisterError,        // 注册失败
    UnsupportedOperation, // 不支持的操作
}

impl Into<SystemError> for DeviceError {
    fn into(self) -> SystemError {
        match self {
            DeviceError::DriverExists => SystemError::EEXIST,
            DeviceError::DeviceExists => SystemError::EEXIST,
            DeviceError::InitializeFailed => SystemError::EIO,
            DeviceError::NotInitialized => SystemError::ENODEV,
            DeviceError::NoDeviceForDriver => SystemError::ENODEV,
            DeviceError::NoDriverForDevice => SystemError::ENODEV,
            DeviceError::RegisterError => SystemError::EIO,
            DeviceError::UnsupportedOperation => SystemError::EIO,
        }
    }
}

/// @brief: 将u32类型转换为设备状态类型
impl From<u32> for DeviceState {
    fn from(state: u32) -> Self {
        match state {
            0 => DeviceState::NotInitialized,
            1 => DeviceState::Initialized,
            _ => todo!(),
        }
    }
}

/// @brief: 将设备状态转换为u32类型
impl From<DeviceState> for u32 {
    fn from(state: DeviceState) -> Self {
        match state {
            DeviceState::NotInitialized => 0,
            DeviceState::Initialized => 1,
            DeviceState::UnDefined => 2,
        }
    }
}

#[derive(Debug)]
pub struct DeviceKObjType;

impl KObjType for DeviceKObjType {
    // https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/core.c#2307
    fn release(&self, kobj: Arc<dyn KObject>) {
        let dev = kobj.cast::<dyn Device>().unwrap();
        /*
         * Some platform devices are driven without driver attached
         * and managed resources may have been acquired.  Make sure
         * all resources are released.
         *
         * Drivers still can add resources into device after device
         * is deleted but alive, so release devres here to avoid
         * possible memory leak.
         */

        // todo: 在引入devres之后再实现
        // devres_release_all(kobj);
        dev.release();
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }

    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&DeviceSysFSOps)
    }
}

#[derive(Debug)]
pub(super) struct DeviceSysFSOps;

impl SysFSOps for DeviceSysFSOps {
    fn store(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        return attr.store(kobj, buf);
    }

    fn show(
        &self,
        kobj: Arc<dyn KObject>,
        attr: &dyn Attribute,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        return attr.show(kobj, buf);
    }
}

/// @brief Device管理器
#[derive(Debug)]
pub struct DeviceManager;

impl DeviceManager {
    /// @brief: 创建一个新的设备管理器
    /// @parameter: None
    /// @return: DeviceManager实体
    #[inline]
    const fn new() -> DeviceManager {
        return Self;
    }

    pub fn register(&self, device: Arc<dyn Device>) -> Result<(), SystemError> {
        self.device_default_initialize(&device);
        return self.add_device(device);
    }

    /// @brief: 添加设备
    /// @parameter id_table: 总线标识符，用于唯一标识该总线
    /// @parameter dev: 设备实例
    /// @return: None
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/core.c#3398
    ///
    /// todo: 完善错误处理逻辑：如果添加失败，需要将之前添加的内容全部回滚
    #[inline(never)]
    #[allow(dead_code)]
    pub fn add_device(&self, device: Arc<dyn Device>) -> Result<(), SystemError> {
        // 在这里处理与parent相关的逻辑

        let current_parent = device
            .parent()
            .map(|x| x.upgrade())
            .flatten()
            .map(|x| x.arc_any().cast::<dyn Device>().ok())
            .flatten();

        let actual_parent = self.get_device_parent(&device, current_parent)?;
        if let Some(actual_parent) = actual_parent {
            // kdebug!(
            //     "device '{}' parent is '{}', strong_count: {}",
            //     device.name().to_string(),
            //     actual_parent.name(),
            //     Arc::strong_count(&actual_parent)
            // );
            device.set_parent(Some(Arc::downgrade(&actual_parent)));
        }

        KObjectManager::add_kobj(device.clone() as Arc<dyn KObject>, None).map_err(|e| {
            kerror!("add device '{:?}' failed: {:?}", device.name(), e);
            e
        })?;

        self.device_platform_notify(&device);

        self.add_class_symlinks(&device)?;

        self.add_attrs(&device)?;

        bus_add_device(&device)?;

        if device.id_table().device_number().major() != Major::UNNAMED_MAJOR {
            self.create_file(&device, &DeviceAttrDev)?;

            self.create_sys_dev_entry(&device)?;
        }

        // 通知客户端有关设备添加的信息。此调用必须在 dpm_sysfs_add() 之后且在 kobject_uevent() 之前执行。
        if let Some(bus) = device.bus().map(|bus| bus.upgrade()).flatten() {
            bus.subsystem().bus_notifier().call_chain(
                bus::BusNotifyEvent::AddDevice,
                Some(&device),
                None,
            );
        }

        // todo: 发送uevent: KOBJ_ADD

        // probe drivers for a new device
        bus_probe_device(&device);

        if let Some(class) = device.class() {
            class.subsystem().add_device_to_vec(&device)?;

            for class_interface in class.subsystem().interfaces() {
                class_interface.add_device(&device).ok();
            }
        }

        return Ok(());
    }

    /// 获取设备真实的parent kobject
    ///
    /// ## 参数
    ///
    /// - `device`: 设备
    /// - `current_parent`: 当前的parent kobject
    ///
    /// ## 返回值
    ///
    /// - `Ok(Some(kobj))`: 如果找到了真实的parent kobject，那么返回它
    /// - `Ok(None)`: 如果没有找到真实的parent kobject，那么返回None
    /// - `Err(e)`: 如果发生错误，那么返回错误
    fn get_device_parent(
        &self,
        device: &Arc<dyn Device>,
        current_parent: Option<Arc<dyn Device>>,
    ) -> Result<Option<Arc<dyn KObject>>, SystemError> {
        // kdebug!("get_device_parent() device:{:?}", device.name());
        if let Some(_) = device.class() {
            let parent_kobj: Arc<dyn KObject>;
            // kdebug!("current_parent:{:?}", current_parent);
            if current_parent.is_none() {
                parent_kobj = sys_devices_virtual_kset() as Arc<dyn KObject>;
            } else {
                let cp = current_parent.unwrap();

                if cp.class().is_some() {
                    return Ok(Some(cp.clone() as Arc<dyn KObject>));
                } else {
                    parent_kobj = cp.clone() as Arc<dyn KObject>;
                }
            }

            // 是否需要glue dir?

            return Ok(Some(parent_kobj));
        }

        // subsystems can specify a default root directory for their devices
        if current_parent.is_none() {
            if let Some(bus) = device.bus().map(|bus| bus.upgrade()).flatten() {
                if let Some(root) = bus.root_device().map(|x| x.upgrade()).flatten() {
                    return Ok(Some(root as Arc<dyn KObject>));
                }
            }
        }

        if current_parent.is_some() {
            return Ok(Some(current_parent.unwrap().clone() as Arc<dyn KObject>));
        }

        return Ok(None);
    }

    /// @brief: 卸载设备
    /// @parameter id_table: 总线标识符，用于唯一标识该设备
    /// @return: None
    ///
    /// ## 注意
    /// 该函数已废弃，不再使用
    #[inline]
    #[allow(dead_code)]
    pub fn remove_device(&self, _id_table: &IdTable) {
        todo!()
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?fi=driver_attach#542
    pub fn remove(&self, _dev: &Arc<dyn Device>) {
        todo!("DeviceManager::remove")
    }

    /// @brief: 获取设备
    /// @parameter id_table: 设备标识符，用于唯一标识该设备
    /// @return: 设备实例
    #[inline]
    #[allow(dead_code)]
    pub fn find_device_by_idtable(&self, _id_table: &IdTable) -> Option<Arc<dyn Device>> {
        todo!("find_device_by_idtable")
    }

    fn device_platform_notify(&self, dev: &Arc<dyn Device>) {
        acpi_device_notify(dev);
        software_node_notify(dev);
    }

    // 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/core.c#3224
    fn add_class_symlinks(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let class = dev.class();
        if class.is_none() {
            return Ok(());
        }

        // 定义错误处理函数，用于在添加符号链接失败时，移除已经添加的符号链接

        let err_remove_device = |dev_kobj: &Arc<dyn KObject>| {
            sysfs_instance().remove_link(dev_kobj, "device".to_string());
        };

        let err_remove_subsystem = |dev_kobj: &Arc<dyn KObject>| {
            sysfs_instance().remove_link(dev_kobj, "subsystem".to_string());
        };

        let class = class.unwrap();
        let dev_kobj = dev.clone() as Arc<dyn KObject>;
        let subsys_kobj = class.subsystem().subsys() as Arc<dyn KObject>;
        sysfs_instance().create_link(Some(&dev_kobj), &subsys_kobj, "subsystem".to_string())?;

        // todo: 这里需要处理class的parent逻辑, 添加device链接
        if let Some(parent) = dev.parent().map(|x| x.upgrade()).flatten() {
            let parent_kobj = parent.clone() as Arc<dyn KObject>;
            sysfs_instance()
                .create_link(Some(&dev_kobj), &&parent_kobj, "device".to_string())
                .map_err(|e| {
                    err_remove_subsystem(&dev_kobj);
                    e
                })?;
        }

        sysfs_instance()
            .create_link(Some(&subsys_kobj), &dev_kobj, dev.name())
            .map_err(|e| {
                err_remove_device(&dev_kobj);
                err_remove_subsystem(&dev_kobj);
                e
            })?;

        return Ok(());
    }

    /// 在sysfs中，为指定的设备创建属性文件
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    fn add_attrs(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        // 定义错误处理函数，用于在添加属性文件失败时，移除已经添加的属性组
        let err_remove_class_groups = |dev: &Arc<dyn Device>| {
            if let Some(class) = dev.class() {
                let attr_groups = class.dev_groups();
                self.remove_groups(dev, attr_groups);
            }
        };

        let err_remove_kobj_type_groups = |dev: &Arc<dyn Device>| {
            if let Some(kobj_type) = dev.kobj_type() {
                let attr_groups = kobj_type.attribute_groups().unwrap_or(&[]);
                self.remove_groups(dev, attr_groups);
            }
        };

        // 真正开始添加属性文件

        // 添加设备类的属性文件
        if let Some(class) = dev.class() {
            let attr_groups = class.dev_groups();
            self.add_groups(dev, attr_groups)?;
        }

        // 添加kobj_type的属性文件
        if let Some(kobj_type) = dev.kobj_type() {
            self.add_groups(dev, kobj_type.attribute_groups().unwrap_or(&[]))
                .map_err(|e| {
                    err_remove_class_groups(dev);
                    e
                })?;
        }

        // 添加设备本身的属性文件
        self.add_groups(dev, dev.attribute_groups().unwrap_or(&[]))
            .map_err(|e| {
                err_remove_kobj_type_groups(dev);
                err_remove_class_groups(dev);
                e
            })?;

        return Ok(());
    }

    /// 在sysfs中，为指定的设备创建属性组，以及属性组中的属性文件
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    /// - `attr_groups`: 属性组
    pub fn add_groups(
        &self,
        dev: &Arc<dyn Device>,
        attr_groups: &'static [&dyn AttributeGroup],
    ) -> Result<(), SystemError> {
        let kobj = dev.clone() as Arc<dyn KObject>;
        return sysfs_instance().create_groups(&kobj, attr_groups);
    }

    /// 在sysfs中，为指定的设备移除属性组，以及属性组中的属性文件
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    /// - `attr_groups`: 要移除的属性组
    pub fn remove_groups(
        &self,
        dev: &Arc<dyn Device>,
        attr_groups: &'static [&dyn AttributeGroup],
    ) {
        let kobj = dev.clone() as Arc<dyn KObject>;
        sysfs_instance().remove_groups(&kobj, attr_groups);
    }

    /// 为设备在sysfs中创建属性文件
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    /// - `attr`: 属性
    pub fn create_file(
        &self,
        dev: &Arc<dyn Device>,
        attr: &'static dyn Attribute,
    ) -> Result<(), SystemError> {
        if unlikely(
            attr.mode().contains(ModeType::S_IRUGO)
                && (!attr.support().contains(SysFSOpsSupport::ATTR_SHOW)),
        ) {
            kwarn!(
                "Attribute '{}': read permission without 'show'",
                attr.name()
            );
        }
        if unlikely(
            attr.mode().contains(ModeType::S_IWUGO)
                && (!attr.support().contains(SysFSOpsSupport::ATTR_STORE)),
        ) {
            kwarn!(
                "Attribute '{}': write permission without 'store'",
                attr.name()
            );
        }

        let kobj = dev.clone() as Arc<dyn KObject>;

        return sysfs_instance().create_file(&kobj, attr);
    }

    /// 在/sys/dev下，或者设备所属的class下，为指定的设备创建链接
    fn create_sys_dev_entry(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let target_kobj = self.device_to_dev_kobj(dev);
        let name = dev.id_table().name();
        let current_kobj = dev.clone() as Arc<dyn KObject>;
        return sysfs_instance().create_link(Some(&target_kobj), &current_kobj, name);
    }

    /// Delete symlink for device in `/sys/dev` or `/sys/class/<class_name>`
    #[allow(dead_code)]
    fn remove_sys_dev_entry(&self, dev: &Arc<dyn Device>) {
        let kobj = self.device_to_dev_kobj(dev);
        let name = dev.id_table().name();
        sysfs_instance().remove_link(&kobj, name);
    }

    /// device_to_dev_kobj - select a /sys/dev/ directory for the device
    ///
    /// By default we select char/ for new entries.
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    fn device_to_dev_kobj(&self, _dev: &Arc<dyn Device>) -> Arc<dyn KObject> {
        // todo: 处理class的逻辑
        let kobj = sys_dev_char_kset().as_kobject();
        return kobj;
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/core.c?fi=device_links_force_bind#1226
    pub fn device_links_force_bind(&self, _dev: &Arc<dyn Device>) {
        todo!("device_links_force_bind")
    }

    /// 把device对象的一些结构进行默认初始化
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/core.c?fi=device_initialize#2976
    pub fn device_default_initialize(&self, dev: &Arc<dyn Device>) {
        dev.set_kset(Some(sys_devices_kset()));
        dev.set_kobj_type(Some(&DeviceKObjType));
        return;
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?r=&mo=29885&fi=1100#1100
    pub fn device_driver_attach(
        &self,
        _driver: &Arc<dyn Driver>,
        _dev: &Arc<dyn Device>,
    ) -> Result<(), SystemError> {
        todo!("device_driver_attach")
    }

    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c?r=&mo=35401&fi=1313#1313
    pub fn device_driver_detach(&self, _dev: &Arc<dyn Device>) {
        todo!("device_driver_detach")
    }
}

/// @brief: 设备注册
/// @parameter: name: 设备名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn device_register<T: Device>(device: Arc<T>) -> Result<(), SystemError> {
    return device_manager().register(device);
}

/// @brief: 设备卸载
/// @parameter: name: 设备名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn device_unregister<T: Device>(_device: Arc<T>) {
    // DEVICE_MANAGER.add_device(device.id_table(), device.clone());
    // match sys_device_unregister(&device.id_table().name()) {
    //     Ok(_) => {
    //         device.set_inode(None);
    //         return Ok(());
    //     }
    //     Err(_) => Err(DeviceError::RegisterError),
    // }
    todo!("device_unregister")
}

/// 设备文件夹下的`dev`文件的属性
#[derive(Debug, Clone, Copy)]
pub struct DeviceAttrDev;

impl Attribute for DeviceAttrDev {
    fn mode(&self) -> ModeType {
        // 0o444
        return ModeType::S_IRUGO;
    }

    fn name(&self) -> &str {
        "dev"
    }

    fn show(&self, kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = kobj.cast::<dyn Device>().map_err(|kobj| {
            kerror!(
                "Intertrait casting not implemented for kobj: {}",
                kobj.name()
            );
            SystemError::EOPNOTSUPP_OR_ENOTSUP
        })?;

        let device_number = dev.id_table().device_number();
        let s = format!(
            "{}:{}\n",
            device_number.major().data(),
            device_number.minor()
        );

        return sysfs_emit_str(buf, &s);
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::ATTR_SHOW
    }
}

/// 设备匹配器
///
/// 用于匹配设备是否符合某个条件
///
/// ## 参数
///
/// - `T` - 匹配器的数据类型
/// - `data` - 匹配器的数据
pub trait DeviceMatcher<T>: Debug {
    fn match_device(&self, device: &Arc<dyn Device>, data: T) -> bool;
}

/// 用于根据名称匹配设备的匹配器
#[derive(Debug)]
pub struct DeviceMatchName;

impl DeviceMatcher<&str> for DeviceMatchName {
    #[inline]
    fn match_device(&self, device: &Arc<dyn Device>, data: &str) -> bool {
        return device.name() == data;
    }
}

/// Cookie to identify the device
#[derive(Debug, Clone, Hash)]
pub struct DeviceId {
    data: Option<&'static str>,
    allocated: Option<String>,
}

impl DeviceId {
    #[allow(dead_code)]
    pub fn new(data: Option<&'static str>, allocated: Option<String>) -> Option<Arc<Self>> {
        if data.is_none() && allocated.is_none() {
            return None;
        }

        // 如果data和allocated都有值，那么返回None
        if data.is_some() && allocated.is_some() {
            return None;
        }

        return Some(Arc::new(Self { data, allocated }));
    }

    pub fn id(&self) -> Option<&str> {
        if self.data.is_some() {
            return Some(self.data.unwrap());
        } else {
            return self.allocated.as_deref();
        }
    }

    #[allow(dead_code)]
    pub fn set_allocated(&mut self, allocated: String) {
        self.allocated = Some(allocated);
        self.data = None;
    }
}

impl PartialEq for DeviceId {
    fn eq(&self, other: &Self) -> bool {
        return self.id() == other.id();
    }
}

impl Eq for DeviceId {}

impl IrqHandlerData for DeviceId {}
