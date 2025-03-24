use core::{any::Any, fmt::Debug};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use bitmap::traits::BitMapOps;
use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::device::blk::{VirtIOBlk, SECTOR_SIZE};

use crate::{
    driver::{
        base::{
            block::{
                block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
                disk_info::Partition,
                manager::{block_dev_manager, BlockDevMeta},
            },
            class::Class,
            device::{
                bus::Bus,
                driver::{Driver, DriverCommonData},
                DevName, Device, DeviceCommonData, DeviceId, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        virtio::{
            sysfs::{virtio_bus, virtio_device_manager, virtio_driver_manager},
            transport::VirtIOTransport,
            virtio_impl::HalImpl,
            VirtIODevice, VirtIODeviceIndex, VirtIODriver, VirtIODriverCommonData, VirtioDeviceId,
            VIRTIO_VENDOR_ID,
        },
    },
    exception::{irqdesc::IrqReturn, IrqNumber},
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
#[allow(dead_code)]
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

pub fn virtio_blk(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    let device = VirtIOBlkDevice::new(transport, dev_id);
    if let Some(device) = device {
        if let Some(dev_parent) = dev_parent {
            device.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
        }
        virtio_device_manager()
            .device_add(device.clone() as Arc<dyn VirtIODevice>)
            .expect("Add virtio blk failed");
    }
}

static mut VIRTIOBLK_MANAGER: Option<VirtIOBlkManager> = None;

#[inline]
fn virtioblk_manager() -> &'static VirtIOBlkManager {
    unsafe { VIRTIOBLK_MANAGER.as_ref().unwrap() }
}

#[unified_init(INITCALL_POSTCORE)]
fn virtioblk_manager_init() -> Result<(), SystemError> {
    unsafe {
        VIRTIOBLK_MANAGER = Some(VirtIOBlkManager::new());
    }
    Ok(())
}

pub struct VirtIOBlkManager {
    inner: SpinLock<InnerVirtIOBlkManager>,
}

struct InnerVirtIOBlkManager {
    id_bmp: bitmap::StaticBitmap<{ VirtIOBlkManager::MAX_DEVICES }>,
    devname: [Option<DevName>; VirtIOBlkManager::MAX_DEVICES],
}

impl VirtIOBlkManager {
    pub const MAX_DEVICES: usize = 25;

    pub fn new() -> Self {
        Self {
            inner: SpinLock::new(InnerVirtIOBlkManager {
                id_bmp: bitmap::StaticBitmap::new(),
                devname: [const { None }; Self::MAX_DEVICES],
            }),
        }
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOBlkManager> {
        self.inner.lock()
    }

    pub fn alloc_id(&self) -> Option<DevName> {
        let mut inner = self.inner();
        let idx = inner.id_bmp.first_false_index()?;
        inner.id_bmp.set(idx, true);
        let name = Self::format_name(idx);
        inner.devname[idx] = Some(name.clone());
        Some(name)
    }

    /// Generate a new block device name like 'vda', 'vdb', etc.
    fn format_name(id: usize) -> DevName {
        let x = (b'a' + id as u8) as char;
        DevName::new(format!("vd{}", x), id)
    }

    #[allow(dead_code)]
    pub fn free_id(&self, id: usize) {
        if id >= Self::MAX_DEVICES {
            return;
        }
        self.inner().id_bmp.set(id, false);
        self.inner().devname[id] = None;
    }
}

/// virtio block device
#[derive(Debug)]
#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIOBlkDevice {
    blkdev_meta: BlockDevMeta,
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIOBlkDevice>,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
}

unsafe impl Send for VirtIOBlkDevice {}
unsafe impl Sync for VirtIOBlkDevice {}

impl VirtIOBlkDevice {
    pub fn new(transport: VirtIOTransport, dev_id: Arc<DeviceId>) -> Option<Arc<Self>> {
        // 设置中断
        if let Err(err) = transport.setup_irq(dev_id.clone()) {
            error!("VirtIOBlkDevice '{dev_id:?}' setup_irq failed: {:?}", err);
            return None;
        }

        let devname = virtioblk_manager().alloc_id()?;
        let irq = Some(transport.irq());
        let device_inner = VirtIOBlk::<HalImpl, VirtIOTransport>::new(transport);
        if let Err(e) = device_inner {
            error!("VirtIOBlkDevice '{dev_id:?}' create failed: {:?}", e);
            return None;
        }

        let mut device_inner: VirtIOBlk<HalImpl, VirtIOTransport> = device_inner.unwrap();
        device_inner.enable_interrupts();
        let dev = Arc::new_cyclic(|self_ref| Self {
            blkdev_meta: BlockDevMeta::new(devname),
            self_ref: self_ref.clone(),
            dev_id,
            locked_kobj_state: LockedKObjectState::default(),
            inner: SpinLock::new(InnerVirtIOBlkDevice {
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

    fn inner(&self) -> SpinLockGuard<InnerVirtIOBlkDevice> {
        self.inner.lock()
    }
}

impl BlockDevice for VirtIOBlkDevice {
    fn dev_name(&self) -> &DevName {
        &self.blkdev_meta.devname
    }

    fn blkdev_meta(&self) -> &BlockDevMeta {
        &self.blkdev_meta
    }

    fn disk_range(&self) -> GeneralBlockRange {
        let inner = self.inner();
        let blocks = inner.device_inner.capacity() as usize * SECTOR_SIZE / LBA_SIZE;
        drop(inner);
        log::debug!(
            "VirtIOBlkDevice '{:?}' disk_range: 0..{}",
            self.dev_name(),
            blocks
        );
        GeneralBlockRange::new(0, blocks).unwrap()
    }

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
                error!(
                    "VirtIOBlkDevice '{:?}' read_at_sync failed: {:?}",
                    self.dev_id, e
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
    irq: Option<IrqNumber>,
}

impl Debug for InnerVirtIOBlkDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerVirtIOBlkDevice").finish()
    }
}

impl VirtIODevice for VirtIOBlkDevice {
    fn irq(&self) -> Option<IrqNumber> {
        self.inner().irq
    }

    fn handle_irq(
        &self,
        _irq: crate::exception::IrqNumber,
    ) -> Result<IrqReturn, system_error::SystemError> {
        // todo: handle virtio blk irq
        Ok(crate::exception::irqdesc::IrqReturn::Handled)
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
        DeviceType::Block
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

    fn dev_parent(&self) -> Option<Weak<dyn Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }

    fn set_dev_parent(&self, parent: Option<Weak<dyn Device>>) {
        self.inner().device_common.parent = parent;
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
        .expect("Add virtio blk driver failed");
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
            virtio_driver_common: VirtIODriverCommonData::default(),
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
        };

        let id_table = VirtioDeviceId::new(
            virtio_drivers::transport::DeviceType::Block as u32,
            VIRTIO_VENDOR_ID.into(),
        );
        let result = VirtIOBlkDriver {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
        };
        result.add_virtio_id(id_table);

        return Arc::new(result);
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOBlkDriver> {
        return self.inner.lock();
    }
}

#[derive(Debug)]
struct InnerVirtIOBlkDriver {
    virtio_driver_common: VirtIODriverCommonData,
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}

impl VirtIODriver for VirtIOBlkDriver {
    fn probe(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOBlkDevice>()
            .map_err(|_| {
                error!(
                "VirtIOBlkDriver::probe() failed: device is not a VirtIO block device. Device: '{:?}'",
                device.name()
            );
                SystemError::EINVAL
            })?;

        block_dev_manager().register(dev as Arc<dyn BlockDevice>)?;
        return Ok(());
    }

    fn virtio_id_table(&self) -> Vec<crate::driver::virtio::VirtioDeviceId> {
        self.inner().virtio_driver_common.id_table.clone()
    }

    fn add_virtio_id(&self, id: VirtioDeviceId) {
        self.inner().virtio_driver_common.id_table.push(id);
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
