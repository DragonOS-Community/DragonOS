use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{any::Any, fmt::Debug, mem::size_of};
use log::{error, warn};
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::{
    queue::VirtQueue,
    transport::{DeviceStatus, DeviceType, Transport},
};

use crate::{
    driver::{
        base::{
            block::{block_device::BlockDevice, manager::block_dev_manager},
            class::Class,
            device::{
                bus::Bus,
                driver::{Driver, DriverCommonData},
                Device, DeviceCommonData, DeviceId, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        block::pmem::{
            register_pmem_region, PmemAccessMode, PmemBlockDevice, PmemFlushOps, PmemRegion,
            PmemRegionSource,
        },
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
        mutex::Mutex,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
        wait_queue::WaitQueue,
    },
    mm::PhysAddr,
    time::{sleep::nanosleep, PosixTimeSpec},
};

const VIRTIO_PMEM_BASENAME: &str = "virtio_pmem";
const VIRTIO_PMEM_FLUSH_QUEUE: u16 = 0;
const VIRTIO_PMEM_REQ_TYPE_FLUSH: u32 = 0;
const VIRTIO_PMEM_QUEUE_SIZE: usize = 4;
const VIRTIO_PMEM_FLUSH_POLL_RETRIES: usize = 1000;
const VIRTIO_PMEM_FLUSH_POLL_INTERVAL_NS: i64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum VirtIOPmemState {
    ReadyNotPublished,
    Published,
    Quiescing,
    Dead,
}

#[repr(C)]
#[derive(Debug)]
struct VirtioPmemReq {
    type_: u32,
}

#[repr(C)]
#[derive(Debug)]
struct VirtioPmemResp {
    ret: u32,
}

#[derive(Debug)]
struct FlushRequest {
    token: u16,
    req: Box<VirtioPmemReq>,
    resp: Box<VirtioPmemResp>,
    done: bool,
    result: Result<(), SystemError>,
}

struct InnerVirtIOPmemDevice {
    transport: Option<VirtIOTransport>,
    flush_queue: Option<VirtQueue<HalImpl, VIRTIO_PMEM_QUEUE_SIZE>>,
    pending_flush: Option<FlushRequest>,
    block_device: Option<Arc<PmemBlockDevice>>,
    state: VirtIOPmemState,
    start: PhysAddr,
    size: usize,
    name: Option<String>,
    virtio_index: Option<VirtIODeviceIndex>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    irq: Option<IrqNumber>,
    irq_is_msix: bool,
}

#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIOPmemDevice {
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIOPmemDevice>,
    locked_kobj_state: LockedKObjectState,
    flush_mutex: Mutex<()>,
    flush_wait: WaitQueue,
}

impl Debug for VirtIOPmemDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtIOPmemDevice")
            .field("dev_id", &self.dev_id.id())
            .finish()
    }
}

unsafe impl Send for VirtIOPmemDevice {}
unsafe impl Sync for VirtIOPmemDevice {}

impl VirtIOPmemDevice {
    pub fn new(mut transport: VirtIOTransport, dev_id: Arc<DeviceId>) -> Option<Arc<Self>> {
        let irq_ready = match transport.setup_irq(dev_id.clone()) {
            Ok(()) => true,
            Err(err) => {
                warn!(
                    "VirtIOPmemDevice '{dev_id:?}' setup_irq failed, falling back to polling: {:?}",
                    err
                );
                false
            }
        };

        begin_pmem_init(&mut transport);
        let (start, size) = match read_config(&transport) {
            Ok(config) => config,
            Err(e) => {
                error!("VirtIOPmemDevice '{dev_id:?}' read config failed: {:?}", e);
                transport.set_status(DeviceStatus::FAILED);
                return None;
            }
        };

        let mut flush_queue = match VirtQueue::<HalImpl, VIRTIO_PMEM_QUEUE_SIZE>::new(
            &mut transport,
            VIRTIO_PMEM_FLUSH_QUEUE,
            false,
            false,
        ) {
            Ok(queue) => queue,
            Err(e) => {
                error!(
                    "VirtIOPmemDevice '{dev_id:?}' create flush queue failed: {:?}",
                    e
                );
                transport.set_status(DeviceStatus::FAILED);
                return None;
            }
        };
        flush_queue.set_dev_notify(true);
        transport.finish_init();

        let irq = irq_ready.then(|| transport.irq());
        let irq_is_msix = irq_ready && transport.irq_is_msix();
        Some(Arc::new(Self {
            dev_id,
            inner: SpinLock::new(InnerVirtIOPmemDevice {
                transport: Some(transport),
                flush_queue: Some(flush_queue),
                pending_flush: None,
                block_device: None,
                state: VirtIOPmemState::ReadyNotPublished,
                start,
                size,
                name: None,
                virtio_index: None,
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
                irq,
                irq_is_msix,
            }),
            locked_kobj_state: LockedKObjectState::default(),
            flush_mutex: Mutex::new(()),
            flush_wait: WaitQueue::default(),
        }))
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOPmemDevice> {
        self.inner.lock_irqsave()
    }

    fn publish_pmem(self: &Arc<Self>) -> Result<(), SystemError> {
        let (start, size) = {
            let inner = self.inner();
            if inner.state != VirtIOPmemState::ReadyNotPublished {
                return Err(SystemError::EINVAL);
            }
            (inner.start, inner.size)
        };

        let pmem = register_pmem_region(PmemRegion {
            start,
            size,
            source: PmemRegionSource::Virtio,
            access: PmemAccessMode::ReadWrite,
            flush: Some(Arc::new(VirtIOPmemFlushOps {
                device: Arc::downgrade(self),
            })),
        })?;

        let mut inner = self.inner();
        inner.block_device = Some(pmem);
        inner.state = VirtIOPmemState::Published;
        Ok(())
    }

    fn fail_unpublished(&self) {
        let mut inner = self.inner();
        inner.state = VirtIOPmemState::Dead;
        inner.pending_flush = None;
        if let Some(transport) = inner.transport.as_mut() {
            transport.set_status(DeviceStatus::FAILED);
            transport.queue_unset(VIRTIO_PMEM_FLUSH_QUEUE);
        }
        self.flush_wait.wakeup_all(None);
    }

    fn flush(&self) -> Result<(), SystemError> {
        let _guard = self.flush_mutex.lock();

        self.wait_for_pending_flush()?;

        let mut request = FlushRequest {
            token: 0,
            req: Box::new(VirtioPmemReq {
                type_: VIRTIO_PMEM_REQ_TYPE_FLUSH.to_le(),
            }),
            resp: Box::new(VirtioPmemResp { ret: 0 }),
            done: false,
            result: Err(SystemError::EIO),
        };

        {
            let mut inner = self.inner();
            if inner.state != VirtIOPmemState::Published {
                return Err(SystemError::EIO);
            }
            if inner
                .transport
                .as_ref()
                .is_some_and(|t| t.get_status().contains(DeviceStatus::DEVICE_NEEDS_RESET))
            {
                return Err(SystemError::EIO);
            }
            if inner.pending_flush.is_some() {
                return Err(SystemError::EBUSY);
            }

            let queue = inner.flush_queue.as_mut().ok_or(SystemError::ENODEV)?;
            let token = unsafe {
                queue.add(
                    &[bytes_of(&*request.req)],
                    &mut [bytes_of_mut(&mut *request.resp)],
                )
            }
            .map_err(virtio_drivers_error_to_system_error)?;
            request.token = token;
            let should_notify = queue.should_notify();
            inner.pending_flush = Some(request);
            if should_notify {
                inner
                    .transport
                    .as_mut()
                    .ok_or(SystemError::ENODEV)?
                    .notify(VIRTIO_PMEM_FLUSH_QUEUE);
            }
        }

        self.wait_for_pending_flush()
    }

    fn wait_for_pending_flush(&self) -> Result<(), SystemError> {
        for _ in 0..VIRTIO_PMEM_FLUSH_POLL_RETRIES {
            {
                let mut inner = self.inner();
                if let Some(result) = self.take_completed_flush_locked(&mut inner) {
                    return result;
                }
                if inner.state >= VirtIOPmemState::Quiescing {
                    return Err(SystemError::EIO);
                }
                if inner.pending_flush.is_none() {
                    return Ok(());
                }
            }
            nanosleep(PosixTimeSpec::new(0, VIRTIO_PMEM_FLUSH_POLL_INTERVAL_NS))?;
        }

        warn!("VirtIOPmem: flush timed out while descriptor is still owned by the device");
        Err(SystemError::ETIMEDOUT)
    }

    fn take_completed_flush_locked(
        &self,
        inner: &mut InnerVirtIOPmemDevice,
    ) -> Option<Result<(), SystemError>> {
        self.complete_flush_locked(inner);
        if inner
            .pending_flush
            .as_ref()
            .is_some_and(|request| request.done)
        {
            return Some(inner.pending_flush.take().unwrap().result);
        }
        None
    }

    fn complete_flush_locked(&self, inner: &mut InnerVirtIOPmemDevice) -> bool {
        let Some(pending) = inner.pending_flush.as_ref() else {
            return false;
        };
        let Some(token) = inner
            .flush_queue
            .as_ref()
            .and_then(|queue| queue.peek_used())
        else {
            return false;
        };
        if token != pending.token {
            return false;
        }

        let mut pending = inner.pending_flush.take().unwrap();
        let result = unsafe {
            inner.flush_queue.as_mut().unwrap().pop_used(
                pending.token,
                &[bytes_of(&*pending.req)],
                &mut [bytes_of_mut(&mut *pending.resp)],
            )
        }
        .map_err(virtio_drivers_error_to_system_error)
        .and_then(|_| {
            if u32::from_le(pending.resp.ret) == 0 {
                Ok(())
            } else {
                Err(SystemError::EIO)
            }
        });

        pending.done = true;
        pending.result = result;
        inner.pending_flush = Some(pending);
        true
    }

    fn shutdown(&self) -> Result<(), SystemError> {
        let block_device = {
            let mut inner = self.inner();
            if inner.state >= VirtIOPmemState::Quiescing {
                return Ok(());
            }
            inner.state = VirtIOPmemState::Quiescing;
            self.flush_wait.wakeup_all(None);
            inner.block_device.take()
        };

        if let Some(block_device) = block_device {
            let block_device = block_device as Arc<dyn BlockDevice>;
            let _ = block_dev_manager().unregister(&block_device);
        }

        let _guard = self.flush_mutex.lock();
        let mut inner = self.inner();
        if let Some(transport) = inner.transport.as_mut() {
            transport.set_status(DeviceStatus::empty());
            transport.queue_unset(VIRTIO_PMEM_FLUSH_QUEUE);
        }
        inner.pending_flush = None;
        inner.state = VirtIOPmemState::Dead;
        Ok(())
    }
}

struct VirtIOPmemFlushOps {
    device: Weak<VirtIOPmemDevice>,
}

impl PmemFlushOps for VirtIOPmemFlushOps {
    fn flush(&self) -> Result<(), SystemError> {
        let device = self.device.upgrade().ok_or(SystemError::ENODEV)?;
        device.flush()
    }
}

impl VirtIODevice for VirtIOPmemDevice {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError> {
        let mut inner = self.inner();
        if inner.state >= VirtIOPmemState::Dead {
            return Ok(IrqReturn::NotHandled);
        }

        let acked = inner
            .transport
            .as_mut()
            .is_some_and(|transport| transport.ack_interrupt());
        if !acked && !inner.irq_is_msix {
            return Ok(IrqReturn::NotHandled);
        }
        let completed = self.complete_flush_locked(&mut inner);
        drop(inner);
        if completed {
            self.flush_wait.wakeup_all(None);
        }
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
            .unwrap_or_else(|| VIRTIO_PMEM_BASENAME.to_string())
    }

    fn set_virtio_device_index(&self, index: VirtIODeviceIndex) {
        self.inner().virtio_index = Some(index);
    }

    fn virtio_device_index(&self) -> Option<VirtIODeviceIndex> {
        self.inner().virtio_index
    }

    fn device_type_id(&self) -> u32 {
        DeviceType::Pmem as u32
    }

    fn vendor(&self) -> u32 {
        VIRTIO_VENDOR_ID.into()
    }

    fn irq(&self) -> Option<IrqNumber> {
        self.inner().irq
    }
}

impl Device for VirtIOPmemDevice {
    fn dev_type(&self) -> crate::driver::base::device::DeviceType {
        crate::driver::base::device::DeviceType::Block
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(VIRTIO_PMEM_BASENAME.to_string(), None)
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner().device_common.bus.clone()
    }

    fn set_bus(&self, bus: Option<Weak<dyn Bus>>) {
        self.inner().device_common.bus = bus;
    }

    fn class(&self) -> Option<Arc<dyn Class>> {
        let mut guard = self.inner();
        let class = guard.device_common.class.clone()?.upgrade();
        if class.is_none() {
            guard.device_common.class = None;
        }
        class
    }

    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }

    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let mut guard = self.inner();
        let driver = guard.device_common.driver.clone()?.upgrade();
        if driver.is_none() {
            guard.device_common.driver = None;
        }
        driver
    }

    fn set_driver(&self, driver: Option<Weak<dyn Driver>>) {
        self.inner().device_common.driver = driver;
    }

    fn is_dead(&self) -> bool {
        self.inner().state == VirtIOPmemState::Dead
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

impl KObject for VirtIOPmemDevice {
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

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }

    fn name(&self) -> String {
        self.device_name()
    }

    fn set_name(&self, name: String) {
        self.set_device_name(name);
    }

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }
}

#[cast_to([sync] VirtIODriver)]
#[cast_to([sync] Driver)]
#[derive(Debug)]
struct VirtIOPmemDriver {
    inner: SpinLock<InnerVirtIOPmemDriver>,
    kobj_state: LockedKObjectState,
}

#[derive(Debug)]
struct InnerVirtIOPmemDriver {
    virtio_driver_common: VirtIODriverCommonData,
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}

impl VirtIOPmemDriver {
    fn new() -> Arc<Self> {
        let driver = Arc::new(Self {
            inner: SpinLock::new(InnerVirtIOPmemDriver {
                virtio_driver_common: VirtIODriverCommonData::default(),
                driver_common: DriverCommonData::default(),
                kobj_common: KObjectCommonData::default(),
            }),
            kobj_state: LockedKObjectState::default(),
        });
        driver.add_virtio_id(VirtioDeviceId::new(
            DeviceType::Pmem as u32,
            VIRTIO_VENDOR_ID.into(),
        ));
        driver
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOPmemDriver> {
        self.inner.lock()
    }
}

impl VirtIODriver for VirtIOPmemDriver {
    fn probe(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOPmemDevice>()
            .map_err(|_| SystemError::EINVAL)?;
        if let Err(e) = dev.publish_pmem() {
            dev.fail_unpublished();
            return Err(e);
        }
        Ok(())
    }

    fn shutdown(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOPmemDevice>()
            .map_err(|_| SystemError::EINVAL)?;
        dev.shutdown()
    }

    fn virtio_id_table(&self) -> Vec<VirtioDeviceId> {
        self.inner().virtio_driver_common.id_table.clone()
    }

    fn add_virtio_id(&self, id: VirtioDeviceId) {
        self.inner().virtio_driver_common.id_table.push(id);
    }
}

impl Driver for VirtIOPmemDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(VIRTIO_PMEM_BASENAME.to_string(), None))
    }

    fn devices(&self) -> Vec<Arc<dyn Device>> {
        self.inner().driver_common.devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        self.inner().driver_common.push_device(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        self.inner().driver_common.delete_device(device);
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        Some(Arc::downgrade(&virtio_bus()) as Weak<dyn Bus>)
    }

    fn set_bus(&self, _bus: Option<Weak<dyn Bus>>) {}
}

impl KObject for VirtIOPmemDriver {
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
        VIRTIO_PMEM_BASENAME.to_string()
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state.write() = state;
    }
}

pub fn virtio_pmem(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    let Some(device) = VirtIOPmemDevice::new(transport, dev_id) else {
        return;
    };
    if let Some(dev_parent) = dev_parent {
        device.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
    }
    virtio_device_manager()
        .device_add(device as Arc<dyn VirtIODevice>)
        .expect("Add virtio pmem failed");
}

#[unified_init(INITCALL_POSTCORE)]
fn virtio_pmem_driver_init() -> Result<(), SystemError> {
    virtio_driver_manager().register(VirtIOPmemDriver::new() as Arc<dyn VirtIODriver>)
}

fn read_config(transport: &VirtIOTransport) -> Result<(PhysAddr, usize), SystemError> {
    let cfg = transport
        .config_space::<[u32; 4]>()
        .map_err(virtio_drivers_error_to_system_error)?;
    let base = cfg.as_ptr() as *const u8;
    let start = read_config_le64(base, 0)?;
    let size = read_config_le64(base, 8)?;
    let start = usize::try_from(start).map_err(|_| SystemError::EOVERFLOW)?;
    let size = usize::try_from(size).map_err(|_| SystemError::EOVERFLOW)?;
    if size == 0 {
        return Err(SystemError::EINVAL);
    }
    start.checked_add(size).ok_or(SystemError::EOVERFLOW)?;
    Ok((PhysAddr::new(start), size))
}

fn read_config_le64(base: *const u8, offset: usize) -> Result<u64, SystemError> {
    let end = offset
        .checked_add(size_of::<u64>())
        .ok_or(SystemError::EOVERFLOW)?;
    if end > 16 {
        return Err(SystemError::EINVAL);
    }
    let mut raw = [0u8; size_of::<u64>()];
    for (idx, byte) in raw.iter_mut().enumerate() {
        *byte = unsafe { core::ptr::read_volatile(base.add(offset + idx)) };
    }
    Ok(u64::from_le_bytes(raw))
}

fn bytes_of<T>(value: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((value as *const T) as *const u8, size_of::<T>()) }
}

fn bytes_of_mut<T>(value: &mut T) -> &mut [u8] {
    unsafe { core::slice::from_raw_parts_mut((value as *mut T) as *mut u8, size_of::<T>()) }
}

fn begin_pmem_init(transport: &mut VirtIOTransport) {
    transport.set_status(DeviceStatus::empty());
    transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);
    let _device_features = transport.read_device_features();
    transport.write_driver_features(0);
    transport
        .set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK);
    transport.set_guest_page_size(virtio_drivers::PAGE_SIZE as u32);
}
