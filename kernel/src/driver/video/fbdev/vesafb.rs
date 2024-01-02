use core::{
    ffi::{c_uint, c_void},
    mem::MaybeUninit,
    sync::atomic::AtomicBool,
};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::MMArch,
    driver::{
        base::{
            class::Class,
            device::{
                bus::Bus, device_manager, driver::Driver, Device, DeviceState, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
            kset::KSet,
            platform::{
                platform_device::{platform_device_manager, PlatformDevice},
                platform_driver::{platform_driver_manager, PlatformDriver},
                CompatibleTable,
            },
        },
        tty::serial::serial8250::send_to_default_serial8250_port,
        video::fbdev::base::{fbmem::frame_buffer_manager, FbVisual},
    },
    filesystem::{
        kernfs::KernFSInode,
        sysfs::{file::sysfs_emit_str, Attribute, AttributeGroup, SysFSOpsSupport},
        vfs::syscall::ModeType,
    },
    include::bindings::bindings::{
        multiboot2_get_Framebuffer_info, multiboot2_iter, multiboot_tag_framebuffer_info_t,
    },
    init::{boot_params, initcall::INITCALL_DEVICE},
    libs::{
        align::page_align_up,
        once::Once,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
    mm::{
        allocator::page_frame::PageFrameCount, no_init::pseudo_map_phys, MemoryManagementArch,
        PhysAddr, VirtAddr,
    },
};

use super::base::{
    fbmem::FbDevice, BlankMode, BootTimeVideoType, FbAccel, FbActivateFlags, FbId, FbState, FbType,
    FbVModeFlags, FbVarScreenInfo, FbVideoMode, FixedScreenInfo, FrameBuffer, FrameBufferInfo,
    FrameBufferOps,
};

/// 当前机器上面是否有vesa帧缓冲区
static HAS_VESA_FB: AtomicBool = AtomicBool::new(false);

lazy_static! {
    static ref VESAFB_FIX_INFO: RwLock<FixedScreenInfo> = RwLock::new(FixedScreenInfo {
        id: FixedScreenInfo::name2id("VESA VGA"),
        fb_type: FbType::PackedPixels,
        accel: FbAccel::None,
        ..Default::default()
    });
    static ref VESAFB_DEFINED: RwLock<FbVarScreenInfo> = RwLock::new(FbVarScreenInfo {
        activate: FbActivateFlags::FB_ACTIVATE_NOW,
        height: None,
        width: None,
        right_margin: 32,
        upper_margin: 16,
        lower_margin: 4,
        vsync_len: 4,
        vmode: FbVModeFlags::FB_VMODE_NONINTERLACED,

        ..Default::default()
    });
}

#[derive(Debug)]
#[cast_to([sync] Device)]
#[cast_to([sync] PlatformDevice)]
pub struct VesaFb {
    inner: SpinLock<InnerVesaFb>,
    kobj_state: LockedKObjectState,
}

impl VesaFb {
    pub const NAME: &'static str = "vesa_vga";
    pub fn new() -> Self {
        return Self {
            inner: SpinLock::new(InnerVesaFb {
                bus: None,
                class: None,
                driver: None,
                kern_inode: None,
                parent: None,
                kset: None,
                kobj_type: None,
                device_state: DeviceState::NotInitialized,
                pdev_id: 0,
                pdev_id_auto: false,
                fb_id: FbId::INIT,
                fb_device: None,
                fb_state: FbState::Suspended,
            }),
            kobj_state: LockedKObjectState::new(None),
        };
    }
}

#[derive(Debug)]
struct InnerVesaFb {
    bus: Option<Weak<dyn Bus>>,
    class: Option<Arc<dyn Class>>,
    driver: Option<Weak<dyn Driver>>,
    kern_inode: Option<Arc<KernFSInode>>,
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
    kobj_type: Option<&'static dyn KObjType>,
    device_state: DeviceState,
    pdev_id: i32,
    pdev_id_auto: bool,
    fb_id: FbId,
    fb_device: Option<Arc<FbDevice>>,
    fb_state: FbState,
}

impl FrameBuffer for VesaFb {
    fn fb_id(&self) -> FbId {
        self.inner.lock().fb_id
    }

    fn set_fb_id(&self, id: FbId) {
        self.inner.lock().fb_id = id;
    }
}

impl PlatformDevice for VesaFb {
    fn pdev_name(&self) -> &str {
        Self::NAME
    }

    fn set_pdev_id(&self, id: i32) {
        self.inner.lock().pdev_id = id;
    }

    fn set_pdev_id_auto(&self, id_auto: bool) {
        self.inner.lock().pdev_id_auto = id_auto;
    }

    fn compatible_table(&self) -> CompatibleTable {
        todo!()
    }

    fn is_initialized(&self) -> bool {
        self.inner.lock().device_state == DeviceState::Initialized
    }

    fn set_state(&self, set_state: DeviceState) {
        self.inner.lock().device_state = set_state;
    }
}

impl Device for VesaFb {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(self.name(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.lock().bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn set_class(&self, class: Option<Arc<dyn Class>>) {
        self.inner.lock().class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        self.inner.lock().driver.clone()?.upgrade()
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner.lock().driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        true
    }

    fn set_can_match(&self, _can_match: bool) {}

    fn state_synced(&self) -> bool {
        true
    }
}

impl KObject for VesaFb {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kern_inode.clone()
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
        self.inner.lock().kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().kobj_type = ktype;
    }

    fn name(&self) -> String {
        Self::NAME.to_string()
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

impl FrameBufferOps for VesaFb {
    fn fb_open(&self, _user: bool) {
        todo!()
    }

    fn fb_release(&self, _user: bool) {
        todo!()
    }

    fn fb_set_color_register(
        &self,
        _regno: u16,
        _red: u16,
        _green: u16,
        _blue: u16,
    ) -> Result<(), SystemError> {
        todo!()
    }

    fn fb_blank(&self, _blank_mode: BlankMode) -> Result<(), SystemError> {
        todo!()
    }

    fn fb_destroy(&self) {
        todo!()
    }

    fn fb_read(&self, buf: &mut [u8], pos: usize) -> Result<usize, SystemError> {
        let bp = boot_params().read();

        let vaddr = bp.screen_info.lfb_virt_base.ok_or(SystemError::ENODEV)?;
        let size = self.current_fb_fix().smem_len;
        drop(bp);
        if pos >= size {
            return Ok(0);
        }

        let pos = pos as i64;
        let size = size as i64;

        let len = core::cmp::min(size - pos, buf.len() as i64) as usize;

        let slice = unsafe { core::slice::from_raw_parts(vaddr.as_ptr::<u8>(), size as usize) };
        buf[..len].copy_from_slice(&slice[pos as usize..(pos as usize + len)]);

        return Ok(len);
    }

    fn fb_write(&self, buf: &[u8], pos: usize) -> Result<usize, SystemError> {
        let bp = boot_params().read();

        let vaddr = bp.screen_info.lfb_virt_base.ok_or(SystemError::ENODEV)?;
        let size = self.current_fb_fix().smem_len;

        if pos >= size {
            return Ok(0);
        }

        let pos = pos as i64;
        let size = size as i64;

        let len = core::cmp::min(size - pos, buf.len() as i64) as usize;

        let slice = unsafe { core::slice::from_raw_parts_mut(vaddr.as_ptr::<u8>(), size as usize) };
        slice[pos as usize..(pos as usize + len)].copy_from_slice(&buf[..len]);

        return Ok(len);
    }
}

impl FrameBufferInfo for VesaFb {
    fn fb_device(&self) -> Option<Arc<FbDevice>> {
        self.inner.lock().fb_device.clone()
    }

    fn set_fb_device(&self, device: Option<Arc<FbDevice>>) {
        self.inner.lock().fb_device = device;
    }

    fn screen_size(&self) -> usize {
        todo!()
    }

    fn current_fb_var(&self) -> FbVarScreenInfo {
        VESAFB_DEFINED.read().clone()
    }

    fn current_fb_fix(&self) -> FixedScreenInfo {
        VESAFB_FIX_INFO.read().clone()
    }

    fn video_mode(&self) -> Option<&FbVideoMode> {
        todo!()
    }

    fn state(&self) -> FbState {
        self.inner.lock().fb_state
    }
}

#[derive(Debug)]
#[cast_to([sync] PlatformDriver)]
struct VesaFbDriver {
    inner: SpinLock<InnerVesaFbDriver>,
    kobj_state: LockedKObjectState,
}

impl VesaFbDriver {
    pub fn new() -> Arc<Self> {
        let r = Arc::new(Self {
            inner: SpinLock::new(InnerVesaFbDriver {
                ktype: None,
                kset: None,
                parent: None,
                kernfs_inode: None,
                devices: Vec::new(),
                bus: None,
                self_ref: Weak::new(),
            }),
            kobj_state: LockedKObjectState::new(None),
        });

        r.inner.lock().self_ref = Arc::downgrade(&r);

        return r;
    }
}

#[derive(Debug)]
struct InnerVesaFbDriver {
    ktype: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
    parent: Option<Weak<dyn KObject>>,
    kernfs_inode: Option<Arc<KernFSInode>>,
    devices: Vec<Arc<dyn Device>>,
    bus: Option<Weak<dyn Bus>>,

    self_ref: Weak<VesaFbDriver>,
}

impl VesaFbDriver {
    const NAME: &'static str = "vesa-framebuffer";
}

impl PlatformDriver for VesaFbDriver {
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        let device = device
            .clone()
            .arc_any()
            .downcast::<VesaFb>()
            .map_err(|_| SystemError::EINVAL)?;

        device.set_driver(Some(self.inner.lock_irqsave().self_ref.clone()));

        return Ok(());
    }

    fn remove(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn shutdown(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        // do nothing
        return Ok(());
    }

    fn suspend(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        // do nothing
        return Ok(());
    }

    fn resume(&self, _device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }
}

impl Driver for VesaFbDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(VesaFb::NAME.to_string(), None))
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner.lock().devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        let mut guard = self.inner.lock();
        // check if the device is already in the list
        if guard.devices.iter().any(|dev| Arc::ptr_eq(dev, &device)) {
            return;
        }

        guard.devices.push(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let mut guard = self.inner.lock();
        guard.devices.retain(|dev| !Arc::ptr_eq(dev, device));
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.lock().bus.clone()
    }

    fn dev_groups(&self) -> &'static [&'static dyn AttributeGroup] {
        return &[&VesaFbAnonAttributeGroup];
    }
}

impl KObject for VesaFbDriver {
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

#[derive(Debug)]
struct VesaFbAnonAttributeGroup;

impl AttributeGroup for VesaFbAnonAttributeGroup {
    fn name(&self) -> Option<&str> {
        None
    }

    fn attrs(&self) -> &[&'static dyn Attribute] {
        &[&AnonAttrPhysAddr as &'static dyn Attribute]
    }

    fn is_visible(
        &self,
        _kobj: Arc<dyn KObject>,
        attr: &'static dyn Attribute,
    ) -> Option<ModeType> {
        Some(attr.mode())
    }
}

#[derive(Debug)]
struct AnonAttrPhysAddr;

impl Attribute for AnonAttrPhysAddr {
    fn name(&self) -> &str {
        "smem_start"
    }

    fn mode(&self) -> ModeType {
        ModeType::S_IRUGO
    }

    fn support(&self) -> SysFSOpsSupport {
        SysFSOpsSupport::SHOW
    }

    fn show(&self, _kobj: Arc<dyn KObject>, buf: &mut [u8]) -> Result<usize, SystemError> {
        sysfs_emit_str(
            buf,
            format!(
                "0x{:x}\n",
                VESAFB_FIX_INFO
                    .read()
                    .smem_start
                    .unwrap_or(PhysAddr::new(0))
                    .data()
            )
            .as_str(),
        )
    }
}

#[unified_init(INITCALL_DEVICE)]
pub fn vesa_fb_driver_init() -> Result<(), SystemError> {
    let driver = VesaFbDriver::new();

    platform_driver_manager().register(driver)?;

    return Ok(());
}

/// 在内存管理初始化之前,初始化vesafb
pub fn vesafb_early_init() -> Result<VirtAddr, SystemError> {
    let mut _reserved: u32 = 0;

    let mut fb_info: MaybeUninit<multiboot_tag_framebuffer_info_t> = MaybeUninit::uninit();
    //从multiboot2中读取帧缓冲区信息至fb_info

    // todo: 换成rust的，并且检测是否成功获取
    unsafe {
        multiboot2_iter(
            Some(multiboot2_get_Framebuffer_info),
            fb_info.as_mut_ptr() as usize as *mut c_void,
            &mut _reserved as *mut c_uint,
        )
    };
    unsafe { fb_info.assume_init() };
    let fb_info: multiboot_tag_framebuffer_info_t = unsafe { core::mem::transmute(fb_info) };

    // todo: 判断是否有vesa帧缓冲区，这里暂时直接设置true
    HAS_VESA_FB.store(true, core::sync::atomic::Ordering::SeqCst);

    let width = fb_info.framebuffer_width;
    let height = fb_info.framebuffer_height;

    let mut boot_params_guard = boot_params().write();
    let boottime_screen_info = &mut boot_params_guard.screen_info;

    boottime_screen_info.is_vga = true;

    boottime_screen_info.lfb_base = PhysAddr::new(fb_info.framebuffer_addr as usize);

    if fb_info.framebuffer_type == 2 {
        //当type=2时,width与height用字符数表示,故depth=8
        boottime_screen_info.origin_video_cols = width as u8;
        boottime_screen_info.origin_video_lines = height as u8;
        boottime_screen_info.video_type = BootTimeVideoType::Mda;
        boottime_screen_info.lfb_depth = 8;
    } else {
        //否则为图像模式,depth应参照帧缓冲区信息里面的每个像素的位数
        boottime_screen_info.lfb_width = width;
        boottime_screen_info.lfb_height = height;
        boottime_screen_info.video_type = BootTimeVideoType::Vlfb;
        boottime_screen_info.lfb_depth = fb_info.framebuffer_bpp as u8;
    }

    boottime_screen_info.lfb_size =
        (width * height * ((fb_info.framebuffer_bpp as u32 + 7) / 8)) as usize;

    let buf_vaddr = VirtAddr::new(0xffff800003200000);
    boottime_screen_info.lfb_virt_base = Some(buf_vaddr);

    let init_text = "Video driver to map.\n\0";
    send_to_default_serial8250_port(init_text.as_bytes());

    // 地址映射
    let paddr = PhysAddr::new(fb_info.framebuffer_addr as usize);
    let count =
        PageFrameCount::new(page_align_up(boottime_screen_info.lfb_size) / MMArch::PAGE_SIZE);
    unsafe { pseudo_map_phys(buf_vaddr, paddr, count) };
    return Ok(buf_vaddr);
}

#[unified_init(INITCALL_DEVICE)]
fn vesa_fb_device_init() -> Result<(), SystemError> {
    // 如果没有vesa帧缓冲区，直接返回
    if !HAS_VESA_FB.load(core::sync::atomic::Ordering::SeqCst) {
        return Ok(());
    }

    static INIT: Once = Once::new();
    INIT.call_once(|| {
        kinfo!("vesa fb device init");

        let mut fix_info_guard = VESAFB_FIX_INFO.write_irqsave();
        let mut var_info_guard = VESAFB_DEFINED.write_irqsave();

        let boot_params_guard = boot_params().read();
        let boottime_screen_info = &boot_params_guard.screen_info;

        fix_info_guard.smem_start = Some(boottime_screen_info.lfb_base);
        fix_info_guard.smem_len = boottime_screen_info.lfb_size;

        if boottime_screen_info.video_type == BootTimeVideoType::Mda {
            fix_info_guard.visual = FbVisual::Mono10;
            var_info_guard.bits_per_pixel = 8;
            fix_info_guard.line_length = (boottime_screen_info.origin_video_cols as u32)
                * (var_info_guard.bits_per_pixel / 8);
            var_info_guard.xres_virtual = boottime_screen_info.origin_video_cols as u32;
            var_info_guard.yres_virtual = boottime_screen_info.origin_video_lines as u32;
        } else {
            fix_info_guard.visual = FbVisual::TrueColor;
            var_info_guard.bits_per_pixel = boottime_screen_info.lfb_depth as u32;
            fix_info_guard.line_length =
                (boottime_screen_info.lfb_width as u32) * (var_info_guard.bits_per_pixel / 8);
            var_info_guard.xres_virtual = boottime_screen_info.lfb_width as u32;
            var_info_guard.yres_virtual = boottime_screen_info.lfb_height as u32;
        }

        drop(var_info_guard);
        drop(fix_info_guard);

        let device = Arc::new(VesaFb::new());
        device_manager().device_default_initialize(&(device.clone() as Arc<dyn Device>));

        platform_device_manager()
            .device_add(device.clone() as Arc<dyn PlatformDevice>)
            .expect("vesa_fb_device_init: platform_device_manager().device_add failed");

        frame_buffer_manager()
            .register_fb(device.clone() as Arc<dyn FrameBuffer>)
            .expect("vesa_fb_device_init: frame_buffer_manager().register_fb failed");

        // 设置vesa fb的状态为运行中
        device.inner.lock().fb_state = FbState::Running;
    });

    return Ok(());
}
