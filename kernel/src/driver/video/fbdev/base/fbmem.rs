use core::intrinsics::unlikely;

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};

use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::{
        class::{class_manager, Class},
        device::{
            bus::Bus,
            device_manager,
            device_number::{DeviceNumber, Major},
            driver::Driver,
            sys_dev_char_kset, Device, DeviceCommonData, DeviceType, IdTable,
        },
        kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
        kset::KSet,
        subsys::SubSysPrivate,
    },
    filesystem::{
        devfs::{devfs_register, DevFS, DeviceINode},
        kernfs::KernFSInode,
        sysfs::AttributeGroup,
        vfs::{
            file::FileMode, syscall::ModeType, FilePrivateData, FileSystem, FileType, IndexNode,
            Metadata,
        },
    },
    init::initcall::INITCALL_SUBSYS,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{fbcon::fb_console_init, fbsysfs::FbDeviceAttrGroup, FbId, FrameBuffer};

/// `/sys/class/graphics` 的 class 实例
static mut CLASS_GRAPHICS_INSTANCE: Option<Arc<GraphicsClass>> = None;

lazy_static! {
    /// 帧缓冲区管理器
    static ref FRAME_BUFFER_MANAGER: FrameBufferManager = FrameBufferManager::new();
}

/// 获取 `/sys/class/graphics` 的 class 实例
#[inline(always)]
#[allow(dead_code)]
pub fn sys_class_graphics_instance() -> Option<&'static Arc<GraphicsClass>> {
    unsafe { CLASS_GRAPHICS_INSTANCE.as_ref() }
}

#[inline(always)]
pub fn frame_buffer_manager() -> &'static FrameBufferManager {
    &FRAME_BUFFER_MANAGER
}

/// 初始化帧缓冲区子系统
#[unified_init(INITCALL_SUBSYS)]
pub fn fbmem_init() -> Result<(), SystemError> {
    let graphics_class = GraphicsClass::new();
    class_manager().class_register(&(graphics_class.clone() as Arc<dyn Class>))?;

    unsafe {
        CLASS_GRAPHICS_INSTANCE = Some(graphics_class);
    }

    fb_console_init()?;
    return Ok(());
}

/// `/sys/class/graphics` 类
#[derive(Debug)]
pub struct GraphicsClass {
    subsystem: SubSysPrivate,
}

impl GraphicsClass {
    const NAME: &'static str = "graphics";
    pub fn new() -> Arc<Self> {
        let r = Self {
            subsystem: SubSysPrivate::new(Self::NAME.to_string(), None, None, &[]),
        };
        let r = Arc::new(r);
        r.subsystem()
            .set_class(Some(Arc::downgrade(&r) as Weak<dyn Class>));

        return r;
    }
}

impl Class for GraphicsClass {
    fn name(&self) -> &'static str {
        return Self::NAME;
    }

    fn dev_kobj(&self) -> Option<Arc<dyn KObject>> {
        Some(sys_dev_char_kset() as Arc<dyn KObject>)
    }

    fn set_dev_kobj(&self, _kobj: Arc<dyn KObject>) {
        unimplemented!("GraphicsClass::set_dev_kobj");
    }

    fn subsystem(&self) -> &SubSysPrivate {
        return &self.subsystem;
    }
}

/// 帧缓冲区管理器
#[derive(Debug)]
pub struct FrameBufferManager {
    inner: RwLock<InnerFrameBufferManager>,
}

#[derive(Debug)]
struct InnerFrameBufferManager {
    /// 已经注册的帧缓冲区
    registered_fbs: [Option<Arc<dyn FrameBuffer>>; FrameBufferManager::FB_MAX],
}

impl FrameBufferManager {
    pub const FB_MAX: usize = 32;
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(InnerFrameBufferManager {
                registered_fbs: Default::default(),
            }),
        }
    }

    /// 注册一个帧缓冲区
    ///
    /// # 参数
    ///
    /// - fb: 帧缓冲区
    pub fn register_fb(&self, fb: Arc<dyn FrameBuffer>) -> Result<FbId, SystemError> {
        let id = self.generate_fb_id().expect("no more fb id");
        fb.set_fb_id(id);
        let fb_device = FbDevice::new(Arc::downgrade(&fb) as Weak<dyn FrameBuffer>, id);
        device_manager().device_default_initialize(&(fb_device.clone() as Arc<dyn Device>));
        fb_device.set_dev_parent(Some(Arc::downgrade(&(fb.clone() as Arc<dyn Device>))));

        fb.set_fb_device(Some(fb_device.clone()));

        device_manager().add_device(fb_device.clone() as Arc<dyn Device>)?;
        // 添加到devfs
        devfs_register(&fb_device.name(), fb_device.clone()).map_err(|e| {
            error!(
                "register fb device '{}' to devfs failed: {:?}",
                fb_device.name(),
                e
            );
            device_manager().remove(&(fb_device.clone() as Arc<dyn Device>));
            e
        })?;

        // todo: 从Modedb中获取信息
        // 参考： https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbmem.c#1584

        let mut inner = self.inner.write();
        inner.registered_fbs[id.data() as usize] = Some(fb.clone() as Arc<dyn FrameBuffer>);

        // todo: 把fb跟fbcon关联起来
        return Ok(id);
    }

    /// 注销一个帧缓冲区
    ///
    /// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbmem.c#1726
    #[allow(dead_code)]
    pub fn unregister_fb(&self, _fb: Arc<dyn FrameBuffer>) -> Result<(), SystemError> {
        todo!("unregister_fb")
    }

    /// 根据id查找帧缓冲区
    #[allow(dead_code)]
    pub fn find_fb_by_id(&self, id: FbId) -> Result<Option<Arc<dyn FrameBuffer>>, SystemError> {
        if unlikely(!id.is_valid()) {
            return Err(SystemError::EINVAL);
        }

        let inner = self.inner.read();
        return Ok(inner.registered_fbs[id.data() as usize].clone());
    }

    fn generate_fb_id(&self) -> Option<FbId> {
        for i in 0..Self::FB_MAX {
            if self.inner.read().registered_fbs[i].is_none() {
                return Some(FbId::new(i as u32));
            }
        }
        return None;
    }
}

/// 抽象的帧缓冲区设备
///
/// 对应于`/sys/class/graphics/fb(x)`目录下的设备, 其中`(x)`为帧缓冲区的id
///
/// 该设备的父设备为真实的帧缓冲区设备
#[derive(Debug)]
#[cast_to([sync] Device)]
pub struct FbDevice {
    inner: SpinLock<InnerFbDevice>,
    kobj_state: LockedKObjectState,
}

impl FbDevice {
    pub const BASENAME: &'static str = "fb";
    fn new(fb: Weak<dyn FrameBuffer>, id: FbId) -> Arc<Self> {
        let r = Arc::new(Self {
            inner: SpinLock::new(InnerFbDevice {
                fb,
                kobject_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                fb_id: id,
                device_inode_fs: None,
                devfs_metadata: Metadata::new(
                    FileType::FramebufferDevice,
                    ModeType::from_bits_truncate(0o666),
                ),
            }),
            kobj_state: LockedKObjectState::new(None),
        });

        let mut inner_guard = r.inner.lock();

        inner_guard.devfs_metadata.raw_dev = r.do_device_number(&inner_guard);
        drop(inner_guard);

        return r;
    }

    pub fn framebuffer(&self) -> Option<Arc<dyn FrameBuffer>> {
        self.inner.lock().fb.upgrade()
    }

    /// 获取设备号
    pub fn device_number(&self) -> DeviceNumber {
        let inner_guard = self.inner.lock();
        self.do_device_number(&inner_guard)
    }

    fn do_device_number(&self, inner_guard: &SpinLockGuard<'_, InnerFbDevice>) -> DeviceNumber {
        DeviceNumber::new(Major::FB_MAJOR, inner_guard.fb_id.data())
    }

    fn inner(&self) -> SpinLockGuard<InnerFbDevice> {
        self.inner.lock()
    }
}

#[derive(Debug)]
struct InnerFbDevice {
    fb: Weak<dyn FrameBuffer>,
    kobject_common: KObjectCommonData,
    device_common: DeviceCommonData,
    /// 帧缓冲区id
    fb_id: FbId,

    /// device inode要求的字段
    device_inode_fs: Option<Weak<DevFS>>,
    devfs_metadata: Metadata,
}

impl KObject for FbDevice {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobject_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobject_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobject_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobject_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobject_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobject_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobject_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        format!("{}{}", Self::BASENAME, self.inner.lock().fb_id.data())
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

impl Device for FbDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(Self::BASENAME.to_string(), Some(self.device_number()))
    }

    fn set_bus(&self, _bus: Option<Weak<dyn Bus>>) {
        todo!()
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        sys_class_graphics_instance().map(|ins| ins.clone() as Arc<dyn Class>)
    }
    fn set_class(&self, _class: Option<Weak<dyn Class>>) {
        // do nothing
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        None
    }

    fn set_driver(&self, _driver: Option<Weak<dyn Driver>>) {
        // do nothing
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        false
    }

    fn set_can_match(&self, _can_match: bool) {
        // do nothing
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        Some(&[&FbDeviceAttrGroup])
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, dev_parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = dev_parent;
    }
}

impl DeviceINode for FbDevice {
    fn set_fs(&self, fs: Weak<DevFS>) {
        self.inner.lock().device_inode_fs = Some(fs);
    }
}

impl IndexNode for FbDevice {
    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let fb = self.inner.lock().fb.upgrade().unwrap();
        return fb.fb_read(&mut buf[0..len], offset);
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let fb = self.inner.lock().fb.upgrade().unwrap();
        return fb.fb_write(&buf[0..len], offset);
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.inner
            .lock()
            .device_inode_fs
            .as_ref()
            .unwrap()
            .upgrade()
            .unwrap()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        todo!()
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.inner.lock().devfs_metadata.clone())
    }

    fn resize(&self, _len: usize) -> Result<(), SystemError> {
        return Ok(());
    }
}
