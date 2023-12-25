use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
};
use system_error::SystemError;

use crate::{
    driver::base::{
        class::Class,
        device::{bus::Bus, device_manager, driver::Driver, Device, DeviceType, IdTable},
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport},
        vfs::syscall::ModeType,
    },
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};

use super::fbmem::sys_class_graphics_instance;

/// framebuffer console设备管理器实例
static mut FB_CONSOLE_MANAGER: Option<FbConsoleManager> = None;

pub fn fb_console_manager() -> &'static FbConsoleManager {
    unsafe { FB_CONSOLE_MANAGER.as_ref().unwrap() }
}

/// 初始化framebuffer console
pub(super) fn fb_console_init() -> Result<(), SystemError> {
    // todo: 对全局的console信号量加锁（linux中是console_lock）

    let fbcon_device: Arc<FbConsoleDevice> = FbConsoleDevice::new();

    {
        let fbcon_manager = FbConsoleManager::new(fbcon_device.clone());
        unsafe { FB_CONSOLE_MANAGER = Some(fbcon_manager) };
    }

    device_manager().register(fbcon_device.clone() as Arc<dyn Device>)?;
    fb_console_manager().init_device()?;

    return Ok(());
}

/// framebuffer console设备管理器
#[derive(Debug)]
pub struct FbConsoleManager {
    _inner: SpinLock<InnerFbConsoleManager>,
    /// framebuffer console设备实例
    /// （对应`/sys/class/graphics/fbcon`）
    device: Arc<FbConsoleDevice>,
}

impl FbConsoleManager {
    pub fn new(device: Arc<FbConsoleDevice>) -> Self {
        return Self {
            _inner: SpinLock::new(InnerFbConsoleManager {}),
            device,
        };
    }

    #[allow(dead_code)]
    #[inline(always)]
    pub fn device(&self) -> &Arc<FbConsoleDevice> {
        &self.device
    }

    /// 初始化设备
    fn init_device(&self) -> Result<(), SystemError> {
        return Ok(()); // todo
    }
}

#[derive(Debug)]
struct InnerFbConsoleManager {}

#[derive(Debug)]
struct InnerFbConsoleDevice {
    kernfs_inode: Option<Arc<KernFSInode>>,
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
    bus: Option<Arc<dyn Bus>>,
    driver: Option<Weak<dyn Driver>>,
    ktype: Option<&'static dyn KObjType>,
}

/// `/sys/class/graphics/fbcon`代表的 framebuffer console 设备
#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct FbConsoleDevice {
    inner: SpinLock<InnerFbConsoleDevice>,
    kobj_state: LockedKObjectState,
}

impl FbConsoleDevice {
    const NAME: &'static str = "fbcon";

    pub fn new() -> Arc<Self> {
        return Arc::new(Self {
            inner: SpinLock::new(InnerFbConsoleDevice {
                kernfs_inode: None,
                parent: None,
                kset: None,
                bus: None,
                ktype: None,
                driver: None,
            }),
            kobj_state: LockedKObjectState::new(None),
        });
    }
}

impl KObject for FbConsoleDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kernfs_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kernfs_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner.lock().parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner.lock().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.lock().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.lock().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.lock().ktype
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().ktype = ktype;
    }

    fn name(&self) -> String {
        Self::NAME.to_string()
    }

    fn set_name(&self, _name: String) {
        // 不允许修改
        kwarn!("fbcon name can not be changed");
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
impl Device for FbConsoleDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(Self::NAME.to_string(), None)
    }

    fn set_bus(&self, bus: Option<Arc<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn set_class(&self, _class: Option<Arc<dyn Class>>) {
        // 不允许修改
        kwarn!("fbcon's class can not be changed");
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        sys_class_graphics_instance().map(|ins| ins.clone() as Arc<dyn Class>)
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner
            .lock()
            .driver
            .clone()
            .and_then(|driver| driver.upgrade())
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner.lock().driver = driver;
    }

    fn is_dead(&self) -> bool {
        todo!()
    }

    fn can_match(&self) -> bool {
        todo!()
    }

    fn set_can_match(&self, _can_match: bool) {
        todo!()
    }

    fn state_synced(&self) -> bool {
        todo!()
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        return Some(&[&AnonymousAttributeGroup]);
    }
}

/// framebuffer console设备的匿名属性组
#[derive(Debug)]
struct AnonymousAttributeGroup;

impl AttributeGroup for AnonymousAttributeGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        return &[&AttrRotate, &AttrRotateAll, &AttrCursorBlink];
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
struct AttrRotate;

impl Attribute for AttrRotate {
    fn name(&self) -> &str {
        "rotate"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::SHOW | SysFSOpsSupport::STORE
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbcon.c#3226
    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        kwarn!("fbcon rotate show not implemented");
        return sysfs_emit_str(buf, "0\n");
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbcon.c#3182
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        kwarn!("fbcon rotate store not implemented");
        return Err(SystemError::ENOSYS);
    }
}

#[derive(Debug)]
struct AttrRotateAll;

impl Attribute for AttrRotateAll {
    fn name(&self) -> &str {
        "rotate_all"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::STORE
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbcon.c#3204
    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        kwarn!("fbcon rotate_all store not implemented");
        return Err(SystemError::ENOSYS);
    }
}

#[derive(Debug)]
struct AttrCursorBlink;

impl Attribute for AttrCursorBlink {
    fn name(&self) -> &str {
        "cursor_blink"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO | ModeType::S_IWUSR
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::SHOW | SysFSOpsSupport::STORE
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbcon.c#3245
    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }
}
