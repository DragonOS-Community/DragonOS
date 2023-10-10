use alloc::{
    string::{String, ToString},
    sync::Arc,
};
use intertrait::cast::CastArc;

use crate::{
    driver::{
        acpi::glue::acpi_device_notify,
        base::map::{LockedDevsMap, LockedKObjMap},
        Driver,
    },
    filesystem::{
        sysfs::{sysfs_instance, Attribute, AttributeGroup, SysFSOps, SysFSOpsSupport},
        vfs::syscall::ModeType,
    },
    syscall::SystemError,
};
use core::fmt::Debug;
use core::intrinsics::unlikely;

use self::bus::{bus_add_device, bus_probe_device, Bus};

use super::{
    kobject::{KObjType, KObject, KObjectManager},
    kset::KSet,
    platform::CompatibleTable,
    swnode::software_node_notify,
};

pub mod bus;
pub mod dd;
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

#[inline(always)]
pub(super) fn sys_devices_kset() -> Arc<KSet> {
    unsafe { DEVICES_KSET_INSTANCE.as_ref().unwrap().clone() }
}

#[inline(always)]
pub(super) fn sys_dev_kset() -> Arc<KSet> {
    unsafe { DEV_KSET_INSTANCE.as_ref().unwrap().clone() }
}

#[inline(always)]
#[allow(dead_code)]
pub(super) fn sys_dev_block_kset() -> Arc<KSet> {
    unsafe { DEV_BLOCK_KSET_INSTANCE.as_ref().unwrap().clone() }
}

#[inline(always)]
pub(self) fn sys_dev_char_kset() -> Arc<KSet> {
    unsafe { DEV_CHAR_KSET_INSTANCE.as_ref().unwrap().clone() }
}

/// 设备应该实现的操作
///
/// ## 注意
///
/// 由于设备驱动模型需要从Arc<dyn KObject>转换为Arc<dyn Device>，
/// 因此，所有的实现了Device trait的结构体，都应该在结构体上方标注`#[[sync] Device]`，
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
    fn bus(&self) -> Option<Arc<dyn Bus>> {
        return None;
    }

    /// 返回已经与当前设备匹配好的驱动程序
    fn driver(&self) -> Option<Arc<dyn Driver>>;

    fn set_driver(&self, driver: Option<Arc<dyn Driver>>);

    /// 当前设备是否已经挂掉了
    fn is_dead(&self) -> bool;
}

// 暂定是不可修改的，在初始化的时候就要确定。以后可能会包括例如硬件中断包含的信息
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct DevicePrivateData {
    id_table: IdTable,
    resource: Option<DeviceResource>,
    compatible_table: CompatibleTable,
    state: DeviceState,
}

impl DevicePrivateData {
    pub fn new(
        id_table: IdTable,
        resource: Option<DeviceResource>,
        compatible_table: CompatibleTable,
        state: DeviceState,
    ) -> Self {
        Self {
            id_table,
            resource,
            compatible_table,
            state,
        }
    }

    pub fn id_table(&self) -> &IdTable {
        &self.id_table
    }

    pub fn state(&self) -> DeviceState {
        self.state
    }

    #[allow(dead_code)]
    pub fn resource(&self) -> Option<&DeviceResource> {
        self.resource.as_ref()
    }

    pub fn compatible_table(&self) -> &CompatibleTable {
        &self.compatible_table
    }

    pub fn set_state(&mut self, state: DeviceState) {
        self.state = state;
    }
}

#[derive(Debug, Clone)]
pub struct DeviceResource {
    //可能会用来保存例如 IRQ PWM 内存地址等需要申请的资源，将来由资源管理器+Framework框架进行管理。
}

impl Default for DeviceResource {
    fn default() -> Self {
        return Self {};
    }
}

int_like!(DeviceNumber, usize);

impl Default for DeviceNumber {
    fn default() -> Self {
        DeviceNumber(0)
    }
}

impl From<usize> for DeviceNumber {
    fn from(dev_t: usize) -> Self {
        DeviceNumber(dev_t)
    }
}

impl Into<usize> for DeviceNumber {
    fn into(self) -> usize {
        self.0
    }
}

impl core::hash::Hash for DeviceNumber {
    fn hash<H: core::hash::Hasher>(&self, state: &mut H) {
        self.0.hash(state);
    }
}

impl DeviceNumber {
    /// @brief: 获取主设备号
    /// @parameter: none
    /// @return: 主设备号
    pub fn major(&self) -> usize {
        (self.0 >> 8) & 0xffffff
    }

    /// @brief: 获取次设备号
    /// @parameter: none
    /// @return: 次设备号
    pub fn minor(&self) -> usize {
        self.0 & 0xff
    }

    pub fn from_major_minor(major: usize, minor: usize) -> usize {
        ((major & 0xffffff) << 8) | (minor & 0xff)
    }
}

/// @brief: 根据主次设备号创建设备号实例
/// @parameter: major: 主设备号
///             minor: 次设备号
/// @return: 设备号实例
pub fn mkdev(major: usize, minor: usize) -> DeviceNumber {
    DeviceNumber(((major & 0xfff) << 20) | (minor & 0xfffff))
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
}

/// @brief: 设备标识符类型
#[derive(Debug, Clone, Hash, PartialOrd, PartialEq, Ord, Eq)]
pub struct IdTable(String, DeviceNumber);

/// @brief: 设备标识符操作方法集
impl IdTable {
    /// @brief: 创建一个新的设备标识符
    /// @parameter name: 设备名
    /// @parameter id: 设备id
    /// @return: 设备标识符
    pub fn new(name: String, id: DeviceNumber) -> IdTable {
        Self(name, id)
    }

    /// @brief: 将设备标识符转换成name
    /// @parameter None
    /// @return: 设备名
    pub fn name(&self) -> String {
        return format!("{}:{}", self.0, self.1 .0);
    }

    pub fn device_number(&self) -> DeviceNumber {
        return self.1;
    }
}

impl Default for IdTable {
    fn default() -> Self {
        IdTable("unknown".to_string(), DeviceNumber::new(0))
    }
}

// 以现在的模型，设备在加载到系统中就是已经初始化的状态了，因此可以考虑把这个删掉
/// @brief: 设备当前状态
#[derive(Debug, Clone, Copy)]
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
    // https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/core.c#2307
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

    /// @brief: 添加设备
    /// @parameter id_table: 总线标识符，用于唯一标识该总线
    /// @parameter dev: 设备实例
    /// @return: None
    ///
    /// https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/core.c#3398
    ///
    /// todo: 完善错误处理逻辑：如果添加失败，需要将之前添加的内容全部回滚
    #[inline]
    #[allow(dead_code)]
    pub fn add_device(&self, device: Arc<dyn Device>) -> Result<(), SystemError> {
        // todo: 引入class后，在这里处理与parent相关的逻辑

        KObjectManager::add_kobj(device.clone() as Arc<dyn KObject>, None).map_err(|e| {
            kerror!("add device '{:?}' failed: {:?}", device.name(), e);
            e
        })?;

        self.device_platform_notify(&device);

        self.add_class_symlinks(&device)?;

        self.add_attrs(&device)?;

        bus_add_device(&device)?;

        if device.id_table().device_number().major() != 0 {
            self.create_file(&device, &DeviceAttrDev)?;

            self.create_sys_dev_entry(&device)?;
        }

        // todo: Notify clients of device addition.This call must come
        //  after dpm_sysfs_add() and before kobject_uevent().
        // 参考：https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/core.c#3491

        // todo: 发送uevent

        // probe drivers for a new device
        bus_probe_device(&device);

        return Ok(());
    }

    /// @brief: 卸载设备
    /// @parameter id_table: 总线标识符，用于唯一标识该设备
    /// @return: None
    #[inline]
    #[allow(dead_code)]
    pub fn remove_device(&self, _id_table: &IdTable) {
        todo!()
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

    fn add_class_symlinks(&self, _dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        // todo: 引入class后，在这里处理与class相关的逻辑
        // https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/core.c#3224

        return Ok(());
    }

    /// 在sysfs中，为指定的设备创建属性文件
    ///
    /// ## 参数
    ///
    /// - `dev`: 设备
    fn add_attrs(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let kobj_type = dev.kobj_type();
        if kobj_type.is_none() {
            return Ok(());
        }

        let kobj_type = kobj_type.unwrap();

        let attr_groups = kobj_type.attribute_groups();

        if attr_groups.is_none() {
            return Ok(());
        }

        self.add_groups(dev, attr_groups.unwrap())?;

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
        let dev = dev.clone();
        let binding = dev.arc_any();
        let kobj: &Arc<dyn KObject> = binding.downcast_ref().unwrap();
        return sysfs_instance().create_groups(kobj, attr_groups);
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
                && (!attr.support().contains(SysFSOpsSupport::SHOW)),
        ) {
            kwarn!(
                "Attribute '{}': read permission without 'show'",
                attr.name()
            );
        }
        if unlikely(
            attr.mode().contains(ModeType::S_IWUGO)
                && (!attr.support().contains(SysFSOpsSupport::STORE)),
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
        return sysfs_instance().create_link(&current_kobj, &target_kobj, name);
    }

    /// Delete symlink for device in `/sys/dev` or `/sys/class/<class_name>`
    #[allow(dead_code)]
    fn remove_sys_dev_entry(&self, dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        let kobj = self.device_to_dev_kobj(dev);
        let name = dev.id_table().name();
        return sysfs_instance().remove_link(&kobj, name);
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

    /// 参考 https://opengrok.ringotek.cn/xref/linux-6.1.9/drivers/base/core.c?fi=device_links_force_bind#1226
    pub fn device_links_force_bind(&self, _dev: &Arc<dyn Device>) {
        todo!("device_links_force_bind")
    }
}

/// @brief: 设备注册
/// @parameter: name: 设备名
/// @return: 操作成功，返回()，操作失败，返回错误码
pub fn device_register<T: Device>(device: Arc<T>) -> Result<(), SystemError> {
    return device_manager().add_device(device);
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

    fn show(&self, kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        let dev = kobj.cast::<dyn Device>().map_err(|kobj| {
            kerror!(
                "Intertrait casting not implemented for kobj: {}",
                kobj.name()
            );
            SystemError::EOPNOTSUPP_OR_ENOTSUP
        })?;

        return Ok(dev.id_table().device_number().into());
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::SHOW
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
