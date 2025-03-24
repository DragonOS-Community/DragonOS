use crate::{
    driver::{
        base::{
            class::Class,
            device::{
                bus::Bus,
                device_number::Major,
                driver::{Driver, DriverCommonData},
                DevName, Device, DeviceCommonData, DeviceId, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        tty::{
            console::ConsoleSwitch,
            kthread::send_to_tty_refresh_thread,
            termios::{WindowSize, TTY_STD_TERMIOS},
            tty_core::{TtyCore, TtyCoreData},
            tty_driver::{TtyDriver, TtyDriverManager, TtyDriverType, TtyOperation},
            virtual_terminal::{vc_manager, virtual_console::VirtualConsoleData, VirtConsole},
        },
        video::console::dummycon::dummy_console,
        virtio::{
            sysfs::{virtio_bus, virtio_device_manager, virtio_driver_manager},
            transport::VirtIOTransport,
            virtio_drivers_error_to_system_error,
            virtio_impl::HalImpl,
            VirtIODevice, VirtIODeviceIndex, VirtIODriver, VirtIODriverCommonData, VirtioDeviceId,
            VIRTIO_VENDOR_ID,
        },
    },
    exception::{irqdesc::IrqReturn, IrqNumber},
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_POSTCORE,
    libs::{
        lazy_init::Lazy,
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use bitmap::traits::BitMapOps;
use core::fmt::Debug;
use core::fmt::Formatter;
use core::{
    any::Any,
    sync::atomic::{compiler_fence, Ordering},
};
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::device::console::VirtIOConsole;

const VIRTIO_CONSOLE_BASENAME: &str = "virtio_console";
const HVC_MINOR: u32 = 0;

static mut VIRTIO_CONSOLE_DRIVER: Option<Arc<VirtIOConsoleDriver>> = None;
static mut TTY_HVC_DRIVER: Option<Arc<TtyDriver>> = None;

#[inline(always)]
fn tty_hvc_driver() -> &'static Arc<TtyDriver> {
    unsafe { TTY_HVC_DRIVER.as_ref().unwrap() }
}

pub fn virtio_console(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    log::debug!(
        "virtio_console: dev_id: {:?}, parent: {:?}",
        dev_id,
        dev_parent
    );
    let device = VirtIOConsoleDevice::new(transport, dev_id.clone());
    if device.is_none() {
        return;
    }

    let device = device.unwrap();

    if let Some(dev_parent) = dev_parent {
        device.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
    }
    virtio_device_manager()
        .device_add(device.clone() as Arc<dyn VirtIODevice>)
        .expect("Add virtio console failed");
}

//
#[derive(Debug)]
#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIOConsoleDevice {
    dev_name: Lazy<DevName>,
    dev_id: Arc<DeviceId>,
    _self_ref: Weak<Self>,
    locked_kobj_state: LockedKObjectState,
    inner: SpinLock<InnerVirtIOConsoleDevice>,
}
unsafe impl Send for VirtIOConsoleDevice {}
unsafe impl Sync for VirtIOConsoleDevice {}

impl VirtIOConsoleDevice {
    pub fn new(transport: VirtIOTransport, dev_id: Arc<DeviceId>) -> Option<Arc<Self>> {
        // 设置中断
        if let Err(err) = transport.setup_irq(dev_id.clone()) {
            log::error!(
                "VirtIOConsoleDevice '{dev_id:?}' setup_irq failed: {:?}",
                err
            );
            return None;
        }

        let irq = Some(transport.irq());
        let device_inner = VirtIOConsole::<HalImpl, VirtIOTransport>::new(transport);
        if let Err(e) = device_inner {
            log::error!("VirtIOConsoleDevice '{dev_id:?}' create failed: {:?}", e);
            return None;
        }

        let mut device_inner: VirtIOConsole<HalImpl, VirtIOTransport> = device_inner.unwrap();
        device_inner.enable_interrupts();

        let dev = Arc::new_cyclic(|self_ref| Self {
            dev_id,
            dev_name: Lazy::new(),
            _self_ref: self_ref.clone(),
            locked_kobj_state: LockedKObjectState::default(),
            inner: SpinLock::new(InnerVirtIOConsoleDevice {
                device_inner,
                name: None,
                virtio_index: None,
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
                irq,
            }),
        });

        Some(dev)
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOConsoleDevice> {
        self.inner.lock_irqsave()
    }
}

struct InnerVirtIOConsoleDevice {
    device_inner: VirtIOConsole<HalImpl, VirtIOTransport>,
    virtio_index: Option<VirtIODeviceIndex>,
    name: Option<String>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    irq: Option<IrqNumber>,
}

impl Debug for InnerVirtIOConsoleDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerVirtIOConsoleDevice")
            .field("virtio_index", &self.virtio_index)
            .field("name", &self.name)
            .field("device_common", &self.device_common)
            .field("kobject_common", &self.kobject_common)
            .field("irq", &self.irq)
            .finish()
    }
}

impl KObject for VirtIOConsoleDevice {
    fn as_any_ref(&self) -> &dyn Any {
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

    fn name(&self) -> String {
        self.device_name()
    }

    fn set_name(&self, _name: String) {
        // do nothing
    }

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }
}

impl Device for VirtIOConsoleDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Char
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(VIRTIO_CONSOLE_BASENAME.to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let r = guard.device_common.class.clone()?.upgrade();
        if r.is_none() {
            guard.device_common.class = None;
        }

        return r;
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }

        return r;
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        false
    }

    fn can_match(&self) -> bool {
        self.inner().device_common.can_match
    }

    fn set_can_match(&self, can_match: bool) {
        self.inner().device_common.can_match = can_match;
    }

    fn state_synced(&self) -> bool {
        true
    }

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl VirtIODevice for VirtIOConsoleDevice {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError> {
        let mut buf = [0u8; 8];
        let mut index = 0;
        // Read up to the size of the buffer
        while index < buf.len() {
            if let Ok(Some(c)) = self.inner().device_inner.recv(true) {
                buf[index] = c;
                index += 1;
            } else {
                break; // No more bytes to read
            }
        }

        send_to_tty_refresh_thread(&buf[0..index]);
        Ok(IrqReturn::Handled)
    }

    fn dev_id(&self) -> &Arc<DeviceId> {
        &self.dev_id
    }

    fn set_device_name(&self, name: String) {
        self.inner().name = Some(name);
    }

    fn device_name(&self) -> String {
        self.inner()
            .name
            .clone()
            .unwrap_or_else(|| VIRTIO_CONSOLE_BASENAME.to_string())
    }

    fn set_virtio_device_index(&self, index: VirtIODeviceIndex) {
        self.inner().virtio_index = Some(index);
    }

    fn virtio_device_index(&self) -> Option<VirtIODeviceIndex> {
        self.inner().virtio_index
    }

    fn device_type_id(&self) -> u32 {
        virtio_drivers::transport::DeviceType::Console as u32
    }

    fn vendor(&self) -> u32 {
        VIRTIO_VENDOR_ID.into()
    }

    fn irq(&self) -> Option<IrqNumber> {
        self.inner().irq
    }
}

#[derive(Debug)]
#[cast_to([sync] VirtIODriver)]
#[cast_to([sync] Driver)]
struct VirtIOConsoleDriver {
    inner: SpinLock<InnerVirtIOConsoleDriver>,
    devices: RwLock<[Option<Arc<VirtIOConsoleDevice>>; Self::MAX_DEVICES]>,
    kobj_state: LockedKObjectState,
}

impl VirtIOConsoleDriver {
    const MAX_DEVICES: usize = 32;

    pub fn new() -> Arc<Self> {
        let inner = InnerVirtIOConsoleDriver {
            virtio_driver_common: VirtIODriverCommonData::default(),
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
            id_bmp: bitmap::StaticBitmap::new(),
            devname: [const { None }; Self::MAX_DEVICES],
        };

        let id_table = VirtioDeviceId::new(
            virtio_drivers::transport::DeviceType::Console as u32,
            VIRTIO_VENDOR_ID.into(),
        );

        let result = VirtIOConsoleDriver {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
            devices: RwLock::new([const { None }; Self::MAX_DEVICES]),
        };

        result.add_virtio_id(id_table);
        Arc::new(result)
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOConsoleDriver> {
        self.inner.lock()
    }

    fn do_install(
        &self,
        driver: Arc<TtyDriver>,
        tty: Arc<TtyCore>,
        vc: Arc<VirtConsole>,
    ) -> Result<(), SystemError> {
        driver.standard_install(tty.clone())?;
        vc.port().setup_internal_tty(Arc::downgrade(&tty));
        tty.set_port(vc.port());
        vc.devfs_setup()?;

        Ok(())
    }
}

#[derive(Debug)]
struct InnerVirtIOConsoleDriver {
    id_bmp: bitmap::StaticBitmap<{ VirtIOConsoleDriver::MAX_DEVICES }>,
    devname: [Option<DevName>; VirtIOConsoleDriver::MAX_DEVICES],
    virtio_driver_common: VirtIODriverCommonData,
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}

impl InnerVirtIOConsoleDriver {
    fn alloc_id(&mut self) -> Option<DevName> {
        let idx = self.id_bmp.first_false_index()?;
        self.id_bmp.set(idx, true);
        let name = Self::format_name(idx);
        self.devname[idx] = Some(name.clone());
        Some(name)
    }

    fn format_name(id: usize) -> DevName {
        DevName::new(format!("vport{}", id), id)
    }

    fn free_id(&mut self, id: usize) {
        if id >= VirtIOConsoleDriver::MAX_DEVICES {
            return;
        }
        self.id_bmp.set(id, false);
        self.devname[id] = None;
    }
}

impl TtyOperation for VirtIOConsoleDriver {
    fn open(&self, _tty: &TtyCoreData) -> Result<(), SystemError> {
        Ok(())
    }

    fn write(&self, tty: &TtyCoreData, buf: &[u8], nr: usize) -> Result<usize, SystemError> {
        if nr > buf.len() {
            return Err(SystemError::EINVAL);
        }
        let index = tty.index();
        if index >= VirtIOConsoleDriver::MAX_DEVICES {
            return Err(SystemError::ENODEV);
        }

        let dev = self.devices.read()[index]
            .clone()
            .ok_or(SystemError::ENODEV)?;
        let mut cnt = 0;
        let mut inner = dev.inner();
        for c in buf[0..nr].iter() {
            if let Err(e) = inner.device_inner.send(*c) {
                if cnt > 0 {
                    return Ok(cnt);
                }
                return Err(virtio_drivers_error_to_system_error(e));
            } else {
                cnt += 1;
            }
        }

        Ok(cnt)
    }

    fn flush_chars(&self, _tty: &TtyCoreData) {
        // do nothing
    }

    fn ioctl(&self, _tty: Arc<TtyCore>, _cmd: u32, _arg: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOIOCTLCMD)
    }

    fn close(&self, _tty: Arc<TtyCore>) -> Result<(), SystemError> {
        Ok(())
    }

    fn resize(&self, tty: Arc<TtyCore>, winsize: WindowSize) -> Result<(), SystemError> {
        *tty.core().window_size_write() = winsize;
        Ok(())
    }

    fn install(&self, driver: Arc<TtyDriver>, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        if tty.core().index() >= VirtIOConsoleDriver::MAX_DEVICES {
            return Err(SystemError::ENODEV);
        }

        let dev = self.devices.read()[tty.core().index()]
            .clone()
            .ok_or(SystemError::ENODEV)?;
        let info = dev.inner().device_inner.info();
        let winsize = WindowSize::new(info.rows, info.columns, 1, 1);

        *tty.core().window_size_write() = winsize;
        let vc_data = Arc::new(SpinLock::new(VirtualConsoleData::new(usize::MAX)));
        let mut vc_data_guard = vc_data.lock_irqsave();
        vc_data_guard.set_driver_funcs(Arc::downgrade(&dummy_console()) as Weak<dyn ConsoleSwitch>);
        vc_data_guard.init(
            Some(tty.core().window_size().row.into()),
            Some(tty.core().window_size().col.into()),
            true,
        );
        drop(vc_data_guard);

        let vc = VirtConsole::new(Some(vc_data));
        let vc_index = vc_manager().alloc(vc.clone()).ok_or(SystemError::EBUSY)?;
        self.do_install(driver, tty, vc.clone()).inspect_err(|_| {
            vc_manager().free(vc_index);
        })?;

        Ok(())
    }
}

impl VirtIODriver for VirtIOConsoleDriver {
    fn probe(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        log::debug!("VirtIOConsoleDriver::probe()");
        let _dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOConsoleDevice>()
            .map_err(|_| {
                log::error!(
                    "VirtIOConsoleDriver::probe() failed: device is not a VirtIO console device. Device: '{:?}'",
                    device.name()
                );
                SystemError::EINVAL
            })?;
        log::debug!("VirtIOConsoleDriver::probe() succeeded");
        Ok(())
    }

    fn virtio_id_table(&self) -> Vec<VirtioDeviceId> {
        self.inner().virtio_driver_common.id_table.clone()
    }

    fn add_virtio_id(&self, id: VirtioDeviceId) {
        self.inner().virtio_driver_common.id_table.push(id);
    }
}

impl Driver for VirtIOConsoleDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(VIRTIO_CONSOLE_BASENAME.to_string(), None))
    }

    // todo: 添加错误时，资源释放的逻辑
    fn add_device(&self, device: Arc<dyn Device>) {
        log::debug!("virtio console: add_device");
        let virtio_con_dev = device.arc_any().downcast::<VirtIOConsoleDevice>().expect(
            "VirtIOConsoleDriver::add_device() failed: device is not a VirtIOConsoleDevice",
        );
        if virtio_con_dev.dev_name.initialized() {
            panic!("VirtIOConsoleDriver::add_device() failed: dev_name has already initialized for device: '{:?}'",
            virtio_con_dev.dev_id(),
        );
        }
        log::debug!("virtio console: add_device: to lock inner");
        let mut inner = self.inner();
        log::debug!("virtio console: add_device: inner.locked");
        let dev_name = inner.alloc_id();
        if dev_name.is_none() {
            panic!("Failed to allocate ID for VirtIO console device: '{:?}', virtio console device limit exceeded.", virtio_con_dev.dev_id())
        }

        let dev_name = dev_name.unwrap();

        virtio_con_dev.dev_name.init(dev_name);

        inner
            .driver_common
            .devices
            .push(virtio_con_dev.clone() as Arc<dyn Device>);

        // avoid deadlock in `init_tty_device`
        drop(inner);

        let mut devices_fast_guard = self.devices.write();
        let index = virtio_con_dev.dev_name.get().id();
        if devices_fast_guard[index].is_none() {
            devices_fast_guard[index] = Some(virtio_con_dev.clone());
        } else {
            panic!("VirtIOConsoleDriver::add_device() failed: device slot already occupied at index: {}", index);
        }
        // avoid deadlock in `init_tty_device`
        drop(devices_fast_guard);

        log::debug!("virtio console: add_device: to init tty device");
        let r = tty_hvc_driver().init_tty_device(Some(index));
        log::debug!(
            "virtio console: add_device: init tty device done, index: {}, dev_name: {:?}",
            index,
            virtio_con_dev.dev_name.get(),
        );
        if let Err(e) = r {
            log::error!(
                "Failed to init tty device for virtio console device, index: {}, dev_name: {:?}, err: {:?}",
                index,
                virtio_con_dev.dev_name.get(),
                e,
            );
            return;
        }
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let virtio_con_dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOConsoleDevice>()
            .expect(
                "VirtIOConsoleDriver::delete_device() failed: device is not a VirtIOConsoleDevice",
            );

        let mut guard = self.inner();
        let mut devices_fast_guard = self.devices.write();
        let index = guard
            .driver_common
            .devices
            .iter()
            .position(|dev| Arc::ptr_eq(device, dev))
            .expect("VirtIOConsoleDriver::delete_device() failed: device not found");

        guard.driver_common.devices.remove(index);
        guard.free_id(virtio_con_dev.dev_name.get().id());

        devices_fast_guard[index] = None;
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner().driver_common.devices.clone()
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        Some(Arc::downgrade(&virtio_bus()) as Weak<dyn Bus>)
    }

    fn set_bus(&self, _bus: Option<Weak<dyn Bus>>) {
        // do nothing
    }
}

impl KObject for VirtIOConsoleDriver {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kobj_common.kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kobj_common.kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().kobj_common.parent.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().kobj_common.parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kobj_common.kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kobj_common.kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner().kobj_common.kobj_type
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobj_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        VIRTIO_CONSOLE_BASENAME.to_string()
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

#[unified_init(INITCALL_POSTCORE)]
fn virtio_console_driver_init() -> Result<(), SystemError> {
    let driver = VirtIOConsoleDriver::new();
    virtio_driver_manager()
        .register(driver.clone() as Arc<dyn VirtIODriver>)
        .expect("Add virtio console driver failed");
    unsafe {
        VIRTIO_CONSOLE_DRIVER = Some(driver.clone());
    }
    let hvc_tty_driver = TtyDriver::new(
        VirtIOConsoleDriver::MAX_DEVICES.try_into().unwrap(),
        "hvc",
        0,
        Major::HVC_MAJOR,
        HVC_MINOR,
        TtyDriverType::System,
        *TTY_STD_TERMIOS,
        driver.clone(),
        None,
    );

    let hvc_tty_driver = TtyDriverManager::tty_register_driver(hvc_tty_driver)?;
    compiler_fence(Ordering::SeqCst);
    unsafe {
        TTY_HVC_DRIVER = Some(hvc_tty_driver);
    }

    compiler_fence(Ordering::SeqCst);

    return Ok(());
}
