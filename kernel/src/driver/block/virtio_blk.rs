use core::{any::Any, fmt::Debug};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::device::blk::VirtIOBlk;

use crate::{
    driver::{
        base::{
            block::{
                block_device::{BlockDevice, BlockId, LBA_SIZE},
                disk_info::Partition,
            },
            class::Class,
            device::{
                bus::Bus,
                driver::{Driver, DriverCommonData},
                Device, DeviceCommonData, DeviceId, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        virtio::{
            sysfs::{virtio_bus, virtio_device_manager, virtio_driver_manager},
            transport::VirtIOTransport,
            virtio_impl::HalImpl,
            VirtIODevice, VirtIODeviceIndex, VirtIODriver, VIRTIO_VENDOR_ID,
        },
    },
    filesystem::{kernfs::KernFSInode, mbr::MbrDiskPartionTable},
    init::initcall::INITCALL_POSTCORE,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
};

const VIRTIO_BLK_BASENAME: &str = "virtio_blk";

static mut VIRTIO_BLK_DRIVER: Option<Arc<VirtIOBlkDriver>> = None;

#[inline(always)]
fn virtio_blk_driver() -> Arc<VirtIOBlkDriver> {
    unsafe { VIRTIO_BLK_DRIVER.as_ref().unwrap().clone() }
}

/// Get the first virtio block device
#[allow(dead_code)]
pub fn virtio_blk_0() -> Option<Arc<VirtIOBlkDevice>> {
    virtio_blk_driver()
        .devices()
        .first()
        .cloned()
        .map(|dev| dev.arc_any().downcast().unwrap())
}

pub fn virtio_blk(transport: VirtIOTransport, dev_id: Arc<DeviceId>) {
    let device = VirtIOBlkDevice::new(transport, dev_id);
    if let Some(device) = device {
        kdebug!("VirtIOBlkDevice '{:?}' created", device.dev_id);
        virtio_device_manager()
            .device_add(device.clone() as Arc<dyn VirtIODevice>)
            .expect("Add virtio blk failed");
    }
}

/// virtio block device
#[derive(Debug)]
#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIOBlkDevice {
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIOBlkDevice>,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
}

unsafe impl Send for VirtIOBlkDevice {}
unsafe impl Sync for VirtIOBlkDevice {}

impl VirtIOBlkDevice {
    pub fn new(transport: VirtIOTransport, dev_id: Arc<DeviceId>) -> Option<Arc<Self>> {
        let device_inner = VirtIOBlk::<HalImpl, VirtIOTransport>::new(transport);
        if let Err(e) = device_inner {
            kerror!("VirtIOBlkDevice '{dev_id:?}' create failed: {:?}", e);
            return None;
        }
        // !!!! 在这里临时测试virtio-blk的读写功能，后续需要删除 !!!!
        // 目前read会报错 `NotReady`
        let device_inner: VirtIOBlk<HalImpl, VirtIOTransport> = device_inner.unwrap();

        let dev = Arc::new_cyclic(|self_ref| Self {
            self_ref: self_ref.clone(),
            dev_id,
            locked_kobj_state: LockedKObjectState::default(),
            inner: SpinLock::new(InnerVirtIOBlkDevice {
                device_inner,
                name: None,
                virtio_index: None,
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
            }),
        });

        dev.set_driver(Some(Arc::downgrade(
            &(virtio_blk_driver() as Arc<dyn Driver>),
        )));

        Some(dev)
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOBlkDevice> {
        self.inner.lock()
    }
}

impl BlockDevice for VirtIOBlkDevice {
    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let mut inner = self.inner();

        inner
            .device_inner
            .read_blocks(lba_id_start, &mut buf[..count * LBA_SIZE])
            .map_err(|e| {
                kerror!(
                    "VirtIOBlkDevice '{:?}' read_at_sync failed: {:?}",
                    self.dev_id,
                    e
                );
                SystemError::EIO
            })?;

        Ok(count)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        self.inner()
            .device_inner
            .write_blocks(lba_id_start, &buf[..count * LBA_SIZE])
            .map_err(|_| SystemError::EIO)?;
        Ok(count)
    }

    fn sync(&self) -> Result<(), SystemError> {
        Ok(())
    }

    fn blk_size_log2(&self) -> u8 {
        9
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn device(&self) -> Arc<dyn Device> {
        self.self_ref.upgrade().unwrap()
    }

    fn block_size(&self) -> usize {
        todo!()
    }

    fn partitions(&self) -> Vec<Arc<Partition>> {
        let device = self.self_ref.upgrade().unwrap() as Arc<dyn BlockDevice>;
        let mbr_table = MbrDiskPartionTable::from_disk(device.clone())
            .expect("Failed to get MBR partition table");
        mbr_table.partitions(Arc::downgrade(&device))
    }
}

struct InnerVirtIOBlkDevice {
    device_inner: VirtIOBlk<HalImpl, VirtIOTransport>,
    name: Option<String>,
    virtio_index: Option<VirtIODeviceIndex>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
}

impl Debug for InnerVirtIOBlkDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerVirtIOBlkDevice").finish()
    }
}

impl VirtIODevice for VirtIOBlkDevice {
    fn handle_irq(
        &self,
        _irq: crate::exception::IrqNumber,
    ) -> Result<crate::exception::irqdesc::IrqReturn, system_error::SystemError> {
        todo!("VirtIOBlkDevice::handle_irq")
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
            .unwrap_or_else(|| VIRTIO_BLK_BASENAME.to_string())
    }

    fn set_virtio_device_index(&self, index: VirtIODeviceIndex) {
        self.inner().virtio_index = Some(index);
    }

    fn virtio_device_index(&self) -> Option<VirtIODeviceIndex> {
        self.inner().virtio_index
    }

    fn device_type_id(&self) -> u32 {
        virtio_drivers::transport::DeviceType::Block as u32
    }

    fn vendor(&self) -> u32 {
        VIRTIO_VENDOR_ID.into()
    }
}

impl Device for VirtIOBlkDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(VIRTIO_BLK_BASENAME.to_string(), None)
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
}

impl KObject for VirtIOBlkDevice {
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

#[unified_init(INITCALL_POSTCORE)]
fn virtio_blk_driver_init() -> Result<(), SystemError> {
    let driver = VirtIOBlkDriver::new();
    virtio_driver_manager()
        .register(driver.clone() as Arc<dyn VirtIODriver>)
        .expect("Add virtio net driver failed");
    unsafe {
        VIRTIO_BLK_DRIVER = Some(driver);
    }

    return Ok(());
}

#[derive(Debug)]
#[cast_to([sync] VirtIODriver)]
#[cast_to([sync] Driver)]
struct VirtIOBlkDriver {
    inner: SpinLock<InnerVirtIOBlkDriver>,
    kobj_state: LockedKObjectState,
}

impl VirtIOBlkDriver {
    pub fn new() -> Arc<Self> {
        let inner = InnerVirtIOBlkDriver {
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
        };
        Arc::new(VirtIOBlkDriver {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
        })
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOBlkDriver> {
        return self.inner.lock();
    }
}

#[derive(Debug)]
struct InnerVirtIOBlkDriver {
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}

impl VirtIODriver for VirtIOBlkDriver {
    fn probe(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let _dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOBlkDevice>()
            .map_err(|_| {
                kerror!(
                "VirtIOBlkDriver::probe() failed: device is not a VirtIO block device. Device: '{:?}'",
                device.name()
            );
                SystemError::EINVAL
            })?;

        return Ok(());
    }
}

impl Driver for VirtIOBlkDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(VIRTIO_BLK_BASENAME.to_string(), None))
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        let iface = device
            .arc_any()
            .downcast::<VirtIOBlkDevice>()
            .expect("VirtIOBlkDriver::add_device() failed: device is not a VirtIOBlkDevice");

        self.inner()
            .driver_common
            .devices
            .push(iface as Arc<dyn Device>);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let _iface = device
            .clone()
            .arc_any()
            .downcast::<VirtIOBlkDevice>()
            .expect("VirtIOBlkDriver::delete_device() failed: device is not a VirtIOBlkDevice");

        let mut guard = self.inner();
        let index = guard
            .driver_common
            .devices
            .iter()
            .position(|dev| Arc::ptr_eq(device, dev))
            .expect("VirtIOBlkDriver::delete_device() failed: device not found");

        guard.driver_common.devices.remove(index);
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

impl KObject for VirtIOBlkDriver {
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
        VIRTIO_BLK_BASENAME.to_string()
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
