use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::{
        base::{
            class::Class,
            device::{bus::Bus, device_manager, driver::Driver, Device, DeviceType, IdTable},
            kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        tty::virtual_terminal::virtual_console::{CursorOperation, VcCursor, VirtualConsoleData},
    },
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport},
        vfs::syscall::ModeType,
    },
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

use super::{fbmem::sys_class_graphics_instance, FbCursor, ScrollMode};

pub mod framebuffer_console;

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
    bus: Option<Weak<dyn Bus>>,
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

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.lock().bus.clone()
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
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
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
        SysFSOpsSupport::ATTR_STORE
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
        SysFSOpsSupport::ATTR_SHOW | SysFSOpsSupport::ATTR_STORE
    }

    /// https://code.dragonos.org.cn/xref/linux-6.1.9/drivers/video/fbdev/core/fbcon.c#3245
    fn show(&self, _kobj: Arc<dyn KObject>, _buf: &mut [u8]) -> Result<usize, SystemError> {
        todo!()
    }

    fn store(&self, _kobj: Arc<dyn KObject>, _buf: &[u8]) -> Result<usize, SystemError> {
        todo!()
    }
}

#[derive(Debug, Default)]
pub struct FrameBufferConsoleData {
    /// 光标闪烁间隔
    pub cursor_blink_jiffies: i64,
    /// 是否刷新光标
    pub cursor_flash: bool,
    ///
    pub display: FbConsoleDisplay,
    /// 光标状态
    pub cursor_state: FbCursor,
    /// 重设光标？
    pub cursor_reset: bool,
    /// cursor 位图数据
    pub cursor_data: Vec<u8>,
}

pub trait FrameBufferConsole {
    fn fbcon_data(&self) -> SpinLockGuard<FrameBufferConsoleData>;

    /// ## 将位块移动到目标位置
    /// 坐标均以字体为单位而不是pixel
    /// ### 参数
    /// ### sy: 起始位置的y坐标
    /// ### sx: 起始位置的x坐标、
    /// ### dy: 目标位置的y坐标
    /// ### dx: 目标位置的x坐标
    /// ### height: 位图高度
    /// ### width: 位图宽度
    fn bmove(
        &self,
        vc_data: &VirtualConsoleData,
        sy: i32,
        sx: i32,
        dy: i32,
        dx: i32,
        height: u32,
        width: u32,
    ) -> Result<(), SystemError>;

    /// ## 清除位图
    ///
    /// ### 参数
    /// ### sy: 原位置的y坐标
    /// ### sx: 原位置的x坐标、
    /// ### height: 位图高度
    /// ### width: 位图宽度
    fn clear(
        &self,
        vc_data: &VirtualConsoleData,
        sy: u32,
        sx: u32,
        height: u32,
        width: u32,
    ) -> Result<(), SystemError>;

    /// ## 显示字符串
    ///
    /// ### 参数
    /// ### y: 起始位置y坐标
    /// ### x: 起始位置的x坐标、
    /// ### fg: 前景色
    /// ### bg: 背景色
    fn put_string(
        &self,
        vc_data: &VirtualConsoleData,
        data: &[u16],
        count: u32,
        y: u32,
        x: u32,
        fg: u32,
        bg: u32,
    ) -> Result<(), SystemError>;

    fn cursor(&self, vc_data: &VirtualConsoleData, op: CursorOperation, fg: u32, bg: u32);
}

/// 表示 framebuffer 控制台与低级帧缓冲设备之间接口的数据结构
#[derive(Debug, Default)]
pub struct FbConsoleDisplay {
    /// 硬件滚动的行数
    pub yscroll: u32,
    /// 光标
    pub cursor_shape: VcCursor,
    /// 滚动模式
    pub scroll_mode: ScrollMode,
    virt_rows: u32,
}

impl FbConsoleDisplay {
    pub fn real_y(&self, mut ypos: u32) -> u32 {
        let rows = self.virt_rows;
        ypos += self.yscroll;
        if ypos < rows {
            return ypos;
        } else {
            return ypos - rows;
        }
    }
}

bitflags! {
    pub struct FbConAttr:u8 {
        const UNDERLINE = 1;
        const REVERSE   = 2;
        const BOLD      = 4;
    }
}

impl FbConAttr {
    pub fn get_attr(c: u16, color_depth: u32) -> Self {
        let mut attr = Self::empty();
        if color_depth == 1 {
            if Self::underline(c) {
                attr.insert(Self::UNDERLINE);
            }
            if Self::reverse(c) {
                attr.intersects(Self::REVERSE);
            }
            if Self::blod(c) {
                attr.insert(Self::BOLD);
            }
        }
        attr
    }

    pub fn update_attr(&self, dst: &mut [u8], src: &[u8], vc_data: &VirtualConsoleData) {
        let mut offset = if vc_data.font.height < 10 { 1 } else { 2 } as usize;

        let width = (vc_data.font.width + 7) / 8;
        let cellsize = (vc_data.font.height * width) as usize;

        // 大于offset的部分就是下划线
        offset = cellsize - (offset * width as usize);
        for i in 0..cellsize {
            let mut c = src[i];
            if self.contains(Self::UNDERLINE) && i >= offset {
                // 下划线
                c = 0xff;
            }
            if self.contains(Self::BOLD) {
                c |= c >> 1;
            }
            if self.contains(Self::REVERSE) {
                c = !c;
            }

            dst[i] = c;
        }
    }

    pub fn underline(c: u16) -> bool {
        c & 0x400 != 0
    }

    pub fn blod(c: u16) -> bool {
        c & 0x200 != 0
    }

    pub fn reverse(c: u16) -> bool {
        c & 0x800 != 0
    }
}
