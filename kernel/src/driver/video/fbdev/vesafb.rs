use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    driver::base::{
        class::Class,
        device::{bus::Bus, driver::Driver, Device, DeviceState, DeviceType, IdTable},
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
        platform::{
            platform_device::PlatformDevice,
            platform_driver::{platform_driver_manager, PlatformDriver},
            CompatibleTable,
        },
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};

use super::base::{
    BlankMode, FbAccel, FbActivateFlags, FbType, FbVModeFlags, FbVarScreenInfo, FbVideoMode,
    FixedScreenInfo, FrameBuffer, FrameBufferInfo, FrameBufferOps,
};

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
pub struct VesaFb {
    inner: SpinLock<InnerVesaFb>,
    kobj_state: LockedKObjectState,
}

impl VesaFb {
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
            }),
            kobj_state: LockedKObjectState::new(None),
        };
    }
}

#[derive(Debug)]
struct InnerVesaFb {
    bus: Option<Arc<dyn Bus>>,
    class: Option<Arc<dyn Class>>,
    driver: Option<Weak<dyn Driver>>,
    kern_inode: Option<Arc<KernFSInode>>,
    parent: Option<Weak<dyn KObject>>,
    kset: Option<Arc<KSet>>,
    kobj_type: Option<&'static dyn KObjType>,
    device_state: DeviceState,
}

impl FrameBuffer for VesaFb {}

impl PlatformDevice for VesaFb {
    fn pdev_name(&self) -> &str {
        todo!()
    }

    fn set_pdev_id(&self, id: i32) {
        todo!()
    }

    fn set_pdev_id_auto(&self, id_auto: bool) {
        todo!()
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

    fn set_bus(&self, bus: Option<Arc<dyn Bus>>) {
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
        let x = VESAFB_FIX_INFO.read().id.map(|x| x as u8);
        String::from_utf8_lossy(&x).to_string()
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
    fn fb_open(&self, user: bool) {
        todo!()
    }

    fn fb_release(&self, user: bool) {
        todo!()
    }

    fn fb_set_color_register(
        &self,
        regno: u16,
        red: u16,
        green: u16,
        blue: u16,
    ) -> Result<(), SystemError> {
        todo!()
    }

    fn fb_blank(&self, blank_mode: BlankMode) -> Result<(), SystemError> {
        todo!()
    }

    fn fb_destroy(&self) {
        todo!()
    }
}

impl FrameBufferInfo for VesaFb {
    fn screen_size(&self) -> usize {
        todo!()
    }

    fn current_fb_var(&self) -> &FbVarScreenInfo {
        todo!()
    }

    fn current_fb_var_mut(&mut self) -> &mut FbVarScreenInfo {
        todo!()
    }

    fn current_fb_fix(&self) -> &FixedScreenInfo {
        todo!()
    }

    fn current_fb_fix_mut(&mut self) -> &mut FixedScreenInfo {
        todo!()
    }

    fn video_mode(&self) -> Option<&FbVideoMode> {
        todo!()
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
        return Arc::new(Self {
            inner: SpinLock::new(InnerVesaFbDriver {
                ktype: None,
                kset: None,
                parent: None,
                kernfs_inode: None,
                devices: Vec::new(),
                bus: None,
            }),
            kobj_state: LockedKObjectState::new(None),
        });
    }
}

#[derive(Debug)]
struct InnerVesaFbDriver {
    ktype: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
    parent: Option<Weak<dyn KObject>>,
    kernfs_inode: Option<Arc<KernFSInode>>,
    devices: Vec<Arc<dyn Device>>,
    bus: Option<Arc<dyn Bus>>,
}

impl VesaFbDriver {
    const NAME: &'static str = "vesa-framebuffer";
}

impl PlatformDriver for VesaFbDriver {
    fn probe(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }

    fn remove(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
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

    fn resume(&self, device: &Arc<dyn PlatformDevice>) -> Result<(), SystemError> {
        todo!()
    }
}

impl Driver for VesaFbDriver {
    fn id_table(&self) -> Option<IdTable> {
        None
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

    fn set_bus(&self, bus: Option<Arc<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn bus(&self) -> Option<Arc<dyn Bus>> {
        self.inner.lock().bus.clone()
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

pub fn vesa_fb_driver_init() -> Result<(), SystemError> {
    let driver = VesaFbDriver::new();

    platform_driver_manager().register(driver)?;

    return Ok(());
}
