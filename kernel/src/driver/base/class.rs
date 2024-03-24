use alloc::{string::ToString, sync::Arc};

use core::fmt::Debug;

use super::{
    device::{sys_dev_char_kset, Device, DeviceMatchName, DeviceMatcher},
    kobject::{KObjType, KObject},
    kset::KSet,
    subsys::SubSysPrivate,
};
use crate::filesystem::sysfs::{sysfs_instance, Attribute, AttributeGroup, SysFSOps};
use system_error::SystemError;

/// `/sys/class`的kset
static mut CLASS_KSET_INSTANCE: Option<Arc<KSet>> = None;

#[inline(always)]
#[allow(dead_code)]
pub fn sys_class_kset() -> Arc<KSet> {
    unsafe { CLASS_KSET_INSTANCE.clone().unwrap() }
}

/// 初始化`/sys/class`的kset
pub(super) fn classes_init() -> Result<(), SystemError> {
    let class_kset = KSet::new("class".to_string());
    class_kset
        .register(None)
        .expect("register class kset failed");
    unsafe {
        CLASS_KSET_INSTANCE = Some(class_kset);
    }

    return Ok(());
}

/// 设备分类
///
/// 类是对设备的高级视图，它抽象了低级实现细节。
///
/// 比如，驱动程序可能看到一个SCSI硬盘或一个ATA硬盘，但在类的这个级别，它们都只是硬盘。
/// 类允许用户空间根据设备的功能而不是它们如何连接或工作来操作设备。
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/device/class.h#54
pub trait Class: Debug + Send + Sync {
    /// 获取类的名称
    fn name(&self) -> &'static str;

    /// 属于该类的设备的基本属性。
    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[];
    }

    /// 当前类的基本属性。
    fn class_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[];
    }

    /// 表示此类的kobject，并将它链接到层次结构中。
    ///
    /// 当前class的所有设备，将会挂载到的`/sys/dev/`内的某个目录下。
    fn dev_kobj(&self) -> Option<Arc<dyn KObject>>;

    fn set_dev_kobj(&self, kobj: Arc<dyn KObject>);

    /// subsystem应当拥有的数据
    fn subsystem(&self) -> &SubSysPrivate;

    /// Called to release this class
    fn class_release(&self) {}
}

impl dyn Class {
    /// 在class内,根据条件寻找一个特定的设备
    ///
    /// ## 参数
    ///
    /// - `matcher` - 匹配器
    /// - `data` - 传给匹配器的数据
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn find_device_by_name(&self, name: &str) -> Option<Arc<dyn Device>> {
        return self.find_device(&DeviceMatchName, name);
    }
}

#[inline(always)]
pub fn class_manager() -> &'static ClassManager {
    return &ClassManager;
}
pub struct ClassManager;

impl ClassManager {
    /// 注册一个设备类
    ///
    /// 该方法会将设备类注册到`/sys/class`目录下，
    /// 并创建它的默认属性组对应的文件。
    ///
    /// ## 参数
    ///
    /// - `class` - 设备类
    pub fn class_register(&self, class: &Arc<dyn Class>) -> Result<(), SystemError> {
        let subsystem = class.subsystem();
        let subsys = subsystem.subsys();
        subsys.set_name(class.name().to_string());

        if class.dev_kobj().is_none() {
            class.set_dev_kobj(sys_dev_char_kset() as Arc<dyn KObject>);
        }

        subsys.set_kobj_type(Some(&ClassKObjbectType));
        subsystem.set_class(Some(Arc::downgrade(class)));

        subsys.register(Some(sys_class_kset()))?;

        sysfs_instance().create_groups(&(subsys as Arc<dyn KObject>), class.class_groups())?;

        return Ok(());
    }

    /// 注销一个设备类
    #[allow(dead_code)]
    pub fn class_unregister(&self, class: &Arc<dyn Class>) {
        let subsystem = class.subsystem();
        let subsys = subsystem.subsys();
        sysfs_instance().remove_groups(&(subsys.clone() as Arc<dyn KObject>), class.class_groups());
        subsys.unregister();
    }
}

#[derive(Debug)]
pub struct ClassKObjbectType;

impl KObjType for ClassKObjbectType {
    fn sysfs_ops(&self) -> Option<&dyn SysFSOps> {
        Some(&ClassSysFSOps)
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}

#[derive(Debug)]
struct ClassSysFSOps;

impl SysFSOps for ClassSysFSOps {
    fn show(
        &self,
        _kobj: Arc<dyn KObject>,
        _attr: &dyn Attribute,
        _buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        todo!()
    }

    fn store(
        &self,
        _kobj: Arc<dyn KObject>,
        _attr: &dyn Attribute,
        _buf: &[u8],
    ) -> Result<usize, SystemError> {
        todo!()
    }
}
