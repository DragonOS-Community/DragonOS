use super::{
    bus::{bus_manager, Bus},
    Device, DeviceMatchName, DeviceMatcher, IdTable,
};
use crate::{
    driver::base::kobject::KObject,
    filesystem::sysfs::{sysfs_instance, Attribute, AttributeGroup},
};
use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::fmt::Debug;
use system_error::SystemError;

/// @brief: Driver error
#[allow(dead_code)]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum DriverError {
    ProbeError,            // 探测设备失败(该驱动不能初始化这个设备)
    RegisterError,         // 设备注册失败
    AllocateResourceError, // 获取设备所需资源失败
    UnsupportedOperation,  // 不支持的操作
    UnInitialized,         // 未初始化
}

impl Into<SystemError> for DriverError {
    fn into(self) -> SystemError {
        match self {
            DriverError::ProbeError => SystemError::ENODEV,
            DriverError::RegisterError => SystemError::ENODEV,
            DriverError::AllocateResourceError => SystemError::EIO,
            DriverError::UnsupportedOperation => SystemError::EIO,
            DriverError::UnInitialized => SystemError::ENODEV,
        }
    }
}

#[inline(always)]
pub fn driver_manager() -> &'static DriverManager {
    &DriverManager
}

/// 驱动程序应当实现的trait
///
/// ## 注意
///
/// 由于设备驱动模型需要从Arc<dyn KObject>转换为Arc<dyn Driver>，
/// 因此，所有的实现了 Driver trait的结构体，都应该在结构体上方标注`#[cast_to([sync] Driver)]`，
/// 否则在运行时会报错
pub trait Driver: Sync + Send + Debug + KObject {
    fn coredump(&self, _device: &Arc<dyn Device>) -> Result<(), SystemError> {
        Err(SystemError::ENOSYS)
    }

    /// @brief: 获取驱动标识符
    /// @parameter: None
    /// @return: 该驱动驱动唯一标识符
    fn id_table(&self) -> Option<IdTable>;

    fn devices(&self) -> Vec<Arc<dyn Device>>;

    /// 把设备加入当前驱动管理的列表中
    fn add_device(&self, device: Arc<dyn Device>);

    /// 从当前驱动管理的列表中删除设备
    fn delete_device(&self, device: &Arc<dyn Device>);

    /// 根据设备名称查找绑定到驱动的设备
    ///
    /// 该方法是一个快速查找方法，要求驱动开发者自行实现。
    ///
    /// 如果开发者没有实现该方法，则应当返回None
    ///
    /// ## 注意
    ///
    /// 这是一个内部方法，不应当被外部调用，若要查找设备，请使用`find_device_by_name()`
    fn __find_device_by_name_fast(&self, _name: &str) -> Option<Arc<dyn Device>> {
        None
    }

    /// 是否禁用sysfs的bind/unbind属性
    ///
    /// ## 返回
    ///
    /// - true: 禁用
    /// - false: 不禁用（默认）
    fn suppress_bind_attrs(&self) -> bool {
        false
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        None
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>);

    fn groups(&self) -> &'static [&'static dyn AttributeGroup] {
        &[]
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        &[]
    }

    /// 使用什么样的策略来探测设备
    fn probe_type(&self) -> DriverProbeType {
        DriverProbeType::DefaultStrategy
    }
}

impl dyn Driver {
    pub fn allows_async_probing(&self) -> bool {
        match self.probe_type() {
            DriverProbeType::PreferAsync => true,
            DriverProbeType::ForceSync => false,
            DriverProbeType::DefaultStrategy => {
                // todo: 判断是否请求异步探测，如果是的话，就返回true

                // 由于目前还没有支持异步探测，因此这里暂时返回false
                false
            }
        }
    }

    /// 根据条件寻找一个绑定到这个驱动的设备(低效实现)
    ///
    /// ## 参数
    ///
    /// - `matcher` - 匹配器
    /// - `data` - 传给匹配器的数据
    ///
    /// ## 注意
    ///
    /// 这里的默认实现很低效，请为特定的驱动自行实现高效的查询
    fn find_device_slow<T: Copy>(
        &self,
        matcher: &dyn DeviceMatcher<T>,
        data: T,
    ) -> Option<Arc<dyn Device>> {
        for dev in self.devices() {
            if matcher.match_device(&dev, data) {
                return Some(dev);
            }
        }

        return None;
    }

    /// 根据设备名称查找绑定到驱动的设备
    ///
    /// ## 注意
    ///
    /// 这里的默认实现很低效，请为特定的驱动自行实现高效的查询
    pub fn find_device_by_name(&self, name: &str) -> Option<Arc<dyn Device>> {
        if let Some(r) = self.__find_device_by_name_fast(name) {
            return Some(r);
        }

        return self.find_device_slow(&DeviceMatchName, name);
    }
}

/// @brief: 驱动管理器
#[derive(Debug, Clone)]
pub struct DriverManager;

impl DriverManager {
    /// 注册设备驱动。该设备驱动应当已经设置好其bus字段
    ///
    /// ## 参数
    ///
    /// - driver: 驱动
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/driver.c#222
    pub fn register(&self, driver: Arc<dyn Driver>) -> Result<(), SystemError> {
        let bus = driver
            .bus()
            .map(|bus| bus.upgrade())
            .flatten()
            .ok_or_else(|| {
                kerror!(
                    "DriverManager::register() failed: driver.bus() is None. Driver: '{:?}'",
                    driver.name()
                );
                SystemError::EINVAL
            })?;

        let drv_name = driver.name();
        let other = bus.find_driver_by_name(&drv_name);
        if other.is_some() {
            kerror!(
                "DriverManager::register() failed: driver '{}' already registered",
                drv_name
            );
            return Err(SystemError::EBUSY);
        }

        bus_manager().add_driver(&driver)?;

        self.add_groups(&driver, driver.groups()).map_err(|e| {
            bus_manager().remove_driver(&driver);
            e
        })?;

        // todo: 发送uevent

        return Ok(());
    }

    /// 从系统中删除一个驱动程序
    #[allow(dead_code)]
    pub fn unregister(&self, driver: &Arc<dyn Driver>) {
        self.remove_groups(driver, driver.groups());
        bus_manager().remove_driver(driver);
    }

    /// 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/base/dd.c#434
    pub fn driver_sysfs_add(&self, _dev: &Arc<dyn Device>) -> Result<(), SystemError> {
        todo!("DriverManager::driver_sysfs_add()");
    }

    pub fn add_groups(
        &self,
        driver: &Arc<dyn Driver>,
        groups: &'static [&dyn AttributeGroup],
    ) -> Result<(), SystemError> {
        let kobj = driver.clone() as Arc<dyn KObject>;
        return sysfs_instance().create_groups(&kobj, groups);
    }

    pub fn remove_groups(&self, driver: &Arc<dyn Driver>, groups: &'static [&dyn AttributeGroup]) {
        let kobj = driver.clone() as Arc<dyn KObject>;
        sysfs_instance().remove_groups(&kobj, groups);
    }

    /// 为指定的驱动创建一个属性文件
    ///
    /// ## 参数
    ///
    /// - `driver` 要创建属性文件的驱动
    /// - `attr` 属性
    pub fn create_attr_file(
        &self,
        driver: &Arc<dyn Driver>,
        attr: &'static dyn Attribute,
    ) -> Result<(), SystemError> {
        let kobj = driver.clone() as Arc<dyn KObject>;
        return sysfs_instance().create_file(&kobj, attr);
    }

    /// 为指定的驱动删除一个属性文件
    ///
    /// 如果属性不存在,也不会报错
    ///
    /// ## 参数
    ///
    /// - `driver` 要删除属性文件的驱动
    /// - `attr` 属性
    pub fn remove_attr_file(&self, driver: &Arc<dyn Driver>, attr: &'static dyn Attribute) {
        let kobj = driver.clone() as Arc<dyn KObject>;
        sysfs_instance().remove_file(&kobj, attr);
    }
}

/// 驱动匹配器
///
/// 用于匹配驱动是否符合某个条件
///
/// ## 参数
///
/// - `T` - 匹配器的数据类型
/// - `data` - 匹配器的数据
pub trait DriverMatcher<T>: Debug {
    fn match_driver(&self, driver: &Arc<dyn Driver>, data: T) -> bool;
}

/// 根据名称匹配驱动
#[derive(Debug)]
pub struct DriverMatchName;

impl DriverMatcher<&str> for DriverMatchName {
    #[inline(always)]
    fn match_driver(&self, driver: &Arc<dyn Driver>, data: &str) -> bool {
        driver.name() == data
    }
}

/// enum probe_type - device driver probe type to try
///	Device drivers may opt in for special handling of their
///	respective probe routines. This tells the core what to
///	expect and prefer.
///
/// Note that the end goal is to switch the kernel to use asynchronous
/// probing by default, so annotating drivers with
/// %PROBE_PREFER_ASYNCHRONOUS is a temporary measure that allows us
/// to speed up boot process while we are validating the rest of the
/// drivers.
#[allow(dead_code)]
#[derive(Debug)]
pub enum DriverProbeType {
    /// Used by drivers that work equally well
    ///	whether probed synchronously or asynchronously.
    DefaultStrategy,

    /// Drivers for "slow" devices which
    ///	probing order is not essential for booting the system may
    ///	opt into executing their probes asynchronously.
    PreferAsync,

    /// Use this to annotate drivers that need
    ///	their probe routines to run synchronously with driver and
    ///	device registration (with the exception of -EPROBE_DEFER
    ///	handling - re-probing always ends up being done asynchronously).
    ForceSync,
}

impl Default for DriverProbeType {
    fn default() -> Self {
        DriverProbeType::DefaultStrategy
    }
}
