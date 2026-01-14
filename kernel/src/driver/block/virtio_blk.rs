use core::{
    any::Any,
    fmt::{Debug, Formatter},
};

use alloc::{
    boxed::Box,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use bitmap::{static_bitmap, traits::BitMapOps};
use hashbrown::HashMap;
use log::error;
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::{
    device::blk::{BlkReq, BlkResp, VirtIOBlk, SECTOR_SIZE},
    Error as VirtioError,
};

use crate::{
    arch::CurrentIrqArch,
    driver::{
        base::{
            block::{
                bio::{BioRequest, BioType},
                bio_queue::BioQueue,
                block_device::{BlockDevice, BlockId, GeneralBlockRange, LBA_SIZE},
                disk_info::Partition,
                manager::{block_dev_manager, BlockDevMeta},
            },
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
        virtio::{
            sysfs::{virtio_bus, virtio_device_manager, virtio_driver_manager},
            transport::VirtIOTransport,
            virtio_impl::HalImpl,
            VirtIODevice, VirtIODeviceIndex, VirtIODriver, VirtIODriverCommonData, VirtioDeviceId,
            VIRTIO_VENDOR_ID,
        },
    },
    exception::{
        irqdesc::IrqReturn,
        tasklet::{tasklet_schedule, Tasklet},
        InterruptArch, IrqNumber,
    },
    filesystem::{
        devfs::{DevFS, DeviceINode, LockedDevFSInode},
        kernfs::KernFSInode,
        mbr::MbrDiskPartionTable,
        vfs::{utils::DName, IndexNode, InodeMode, Metadata},
    },
    init::initcall::INITCALL_POSTCORE,
    libs::{
        mutex::MutexGuard,
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    sched::prio::MAX_RT_PRIO,
    time::{sleep::nanosleep, PosixTimeSpec},
};

const VIRTIO_BLK_BASENAME: &str = "virtio_blk";

// IO线程的budget配置
const IO_BUDGET: usize = 32; // 每次最多处理32个请求
const SLEEP_MS: usize = 20; // 达到budget后睡眠20ms

/// Token映射表：virtqueue token -> BioRequest
/// BIO请求的完整上下文，包含virtio需要的req和resp
struct BioContext {
    bio: Arc<BioRequest>,
    req: Box<BlkReq>,
    resp: Box<BlkResp>,
}

struct BioTokenMap {
    inner: SpinLock<HashMap<u16, BioContext>>,
}

impl BioTokenMap {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: SpinLock::new(HashMap::new()),
        })
    }

    pub fn insert(&self, token: u16, ctx: BioContext) -> Result<(), BioContext> {
        let mut inner = self.inner.lock_irqsave();
        if inner.contains_key(&token) {
            return Err(ctx);
        }
        inner.insert(token, ctx);
        Ok(())
    }

    pub fn remove(&self, token: u16) -> Option<BioContext> {
        self.inner.lock_irqsave().remove(&token)
    }
}

static mut VIRTIO_BLK_DRIVER: Option<Arc<VirtIOBlkDriver>> = None;

/// 中断下半部：完成 BIO 请求
struct BioCompletionTasklet {
    token_map: Arc<BioTokenMap>,
    device: Weak<VirtIOBlkDevice>,
    tasklet: Arc<Tasklet>,
}

impl BioCompletionTasklet {
    fn new(device: Weak<VirtIOBlkDevice>, token_map: Arc<BioTokenMap>) -> Arc<Self> {
        Arc::new_cyclic(|weak: &alloc::sync::Weak<Self>| {
            let weak_for_cb = weak.clone();
            let tasklet = Tasklet::new(
                move |_, _| {
                    if let Some(tasklet) = weak_for_cb.upgrade() {
                        tasklet.run();
                    }
                },
                0,
                None,
            );
            BioCompletionTasklet {
                token_map,
                device,
                tasklet,
            }
        })
    }

    fn schedule(&self) {
        tasklet_schedule(&self.tasklet);
    }

    fn run(&self) {
        let device = match self.device.upgrade() {
            Some(dev) => dev,
            None => return,
        };
        loop {
            let mut inner = device.inner();
            let token = match inner.device_inner.peek_used() {
                Some(token) => token,
                None => break,
            };

            let ctx = match self.token_map.remove(token) {
                Some(ctx) => ctx,
                None => {
                    error!("VirtIOBlk: token {} not found in token_map", token);
                    continue;
                }
            };

            let mut ctx = ctx;
            let bio = ctx.bio.clone();
            let count = bio.count();
            let result = match bio.bio_type() {
                BioType::Read => {
                    let buf_ptr = bio.buffer_mut();
                    let buf = unsafe { &mut *buf_ptr };
                    match unsafe {
                        inner.device_inner.complete_read_blocks(
                            token,
                            &ctx.req,
                            &mut buf[..count * LBA_SIZE],
                            &mut ctx.resp,
                        )
                    } {
                        Ok(_) => Ok(count * LBA_SIZE),
                        Err(VirtioError::NotReady) => {
                            if self.token_map.insert(token, ctx).is_err() {
                                error!("VirtIOBlk: token {} reinsert failed", token);
                                bio.complete(Err(SystemError::EIO));
                            }
                            break;
                        }
                        Err(e) => {
                            error!("VirtIOBlk complete_read_blocks failed: {:?}", e);
                            Err(SystemError::EIO)
                        }
                    }
                }
                BioType::Write => {
                    let buf_ptr = bio.buffer();
                    let buf = unsafe { &*buf_ptr };
                    match unsafe {
                        inner.device_inner.complete_write_blocks(
                            token,
                            &ctx.req,
                            &buf[..count * LBA_SIZE],
                            &mut ctx.resp,
                        )
                    } {
                        Ok(_) => Ok(count * LBA_SIZE),
                        Err(VirtioError::NotReady) => {
                            if self.token_map.insert(token, ctx).is_err() {
                                error!("VirtIOBlk: token {} reinsert failed", token);
                                bio.complete(Err(SystemError::EIO));
                            }
                            break;
                        }
                        Err(e) => {
                            error!("VirtIOBlk complete_write_blocks failed: {:?}", e);
                            Err(SystemError::EIO)
                        }
                    }
                }
            };
            drop(inner);
            bio.complete(result);
        }
    }
}

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
    id_bmp: static_bitmap!(VirtIOBlkManager::MAX_DEVICES),
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

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOBlkManager> {
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
#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIOBlkDevice {
    blkdev_meta: BlockDevMeta,
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIOBlkDevice>,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
    parent: RwLock<Weak<LockedDevFSInode>>,
    fs: RwLock<Weak<DevFS>>,
    metadata: Metadata,
}

impl Debug for VirtIOBlkDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtIOBlkDevice")
            .field("devname", &self.blkdev_meta.devname)
            .field("dev_id", &self.dev_id.id())
            .finish()
    }
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

        // 创建BIO队列和token映射表
        let bio_queue = BioQueue::new();
        let bio_token_map = BioTokenMap::new();

        let dev = Arc::new_cyclic(|self_ref| Self {
            blkdev_meta: BlockDevMeta::new(devname.clone(), Major::VIRTIO_BLK_MAJOR),
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
                bio_queue: Some(bio_queue.clone()),
                bio_token_map: Some(bio_token_map.clone()),
                io_thread_pcb: None, // 稍后初始化
                completion_tasklet: None,
            }),
            parent: RwLock::new(Weak::default()),
            fs: RwLock::new(Weak::default()),
            metadata: Metadata::new(
                crate::filesystem::vfs::FileType::BlockDevice,
                InodeMode::from_bits_truncate(0o755),
            ),
        });

        let device_weak = Arc::downgrade(&dev);

        // 创建BIO完成 tasklet
        let completion_tasklet = BioCompletionTasklet::new(device_weak.clone(), bio_token_map);
        dev.inner().completion_tasklet = Some(completion_tasklet);

        // 创建IO线程
        let thread_name = format!("virtio_blk_io_{}", devname.id());
        let io_thread = KernelThreadMechanism::create_and_run(
            KernelThreadClosure::EmptyClosure((
                alloc::boxed::Box::new(move || bio_io_thread_loop(device_weak.clone())),
                (),
            )),
            thread_name.clone(),
        );

        if let Some(io_thread) = io_thread {
            // 设置FIFO调度策略
            if let Err(err) = ProcessManager::set_fifo_policy(&io_thread, MAX_RT_PRIO - 1) {
                error!("Failed to set FIFO policy for {}: {:?}", thread_name, err);
            }

            // 保存IO线程PCB
            dev.inner().io_thread_pcb = Some(io_thread.clone());
        } else {
            error!("Failed to create IO thread for {}", thread_name);
            virtioblk_manager().free_id(devname.id());
            return None;
        }

        Some(dev)
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOBlkDevice> {
        self.inner.lock_irqsave()
    }
}

impl IndexNode for VirtIOBlkDevice {
    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.fs
            .read()
            .upgrade()
            .expect("VirtIOBlkDevice fs is not set")
    }
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn list(&self) -> Result<alloc::vec::Vec<alloc::string::String>, system_error::SystemError> {
        Err(SystemError::ENOSYS)
    }
    fn metadata(&self) -> Result<crate::filesystem::vfs::Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let parent = self.parent.read();
        if let Some(parent) = parent.upgrade() {
            return Ok(parent as Arc<dyn IndexNode>);
        }
        Err(SystemError::ENOENT)
    }

    fn close(
        &self,
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn dname(&self) -> Result<DName, SystemError> {
        let dname = DName::from(self.blkdev_meta.devname.clone().as_ref());
        Ok(dname)
    }

    fn open(
        &self,
        _data: MutexGuard<crate::filesystem::vfs::FilePrivateData>,
        _mode: &crate::filesystem::vfs::file::FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }
}

impl DeviceINode for VirtIOBlkDevice {
    fn set_fs(&self, fs: alloc::sync::Weak<crate::filesystem::devfs::DevFS>) {
        *self.fs.write() = fs;
    }

    fn set_parent(&self, parent: Weak<crate::filesystem::devfs::LockedDevFSInode>) {
        *self.parent.write() = parent;
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
        GeneralBlockRange::new(0, blocks).unwrap()
    }

    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        let bio = self.submit_bio_read(lba_id_start, count)?;
        let data = bio
            .wait()
            .inspect_err(|e| log::error!("VirtIOBlkDevice read_at_sync error: {:?}", e))?;
        buf[..count * LBA_SIZE].copy_from_slice(&data[..count * LBA_SIZE]);
        Ok(count)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        let bio = self.submit_bio_write(lba_id_start, count, &buf[..count * LBA_SIZE])?;
        let _ = bio
            .wait()
            .inspect_err(|e| log::error!("VirtIOBlkDevice write_at_sync error: {:?}", e))?;
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

    /// 提交异步BIO请求
    fn submit_bio(&self, bio: Arc<BioRequest>) -> Result<(), SystemError> {
        let inner = self.inner();
        if let Some(bio_queue) = &inner.bio_queue {
            bio_queue.submit(bio);
            Ok(())
        } else {
            Err(SystemError::ENOSYS)
        }
    }
}

struct InnerVirtIOBlkDevice {
    device_inner: VirtIOBlk<HalImpl, VirtIOTransport>,
    name: Option<String>,
    virtio_index: Option<VirtIODeviceIndex>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    irq: Option<IrqNumber>,
    // 异步IO支持（阶段2新增）
    bio_queue: Option<Arc<BioQueue>>,
    bio_token_map: Option<Arc<BioTokenMap>>, // 阶段3将使用
    io_thread_pcb: Option<Arc<ProcessControlBlock>>,
    completion_tasklet: Option<Arc<BioCompletionTasklet>>,
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
        let mut inner = self.inner();
        if !inner.device_inner.ack_interrupt() {
            return Ok(crate::exception::irqdesc::IrqReturn::NotHandled);
        }
        let tasklet = inner.completion_tasklet.clone();
        if let Some(tasklet) = tasklet {
            tasklet.schedule();
        }
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
        self.blkdev_meta.inner().dev_idx = index.into();
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

    fn kobj_state(&self) -> RwSemReadGuard<'_, KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwSemWriteGuard<'_, KObjectState> {
        self.locked_kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.locked_kobj_state.write() = state;
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner().kobject_common.kobj_type = ktype;
    }
}

impl Drop for VirtIOBlkDevice {
    fn drop(&mut self) {
        let (bio_queue, bio_token_map) = {
            let inner = self.inner.lock_irqsave();
            (inner.bio_queue.clone(), inner.bio_token_map.clone())
        };

        if let Some(bio_queue) = bio_queue {
            loop {
                let batch = bio_queue.drain_batch();
                if batch.is_empty() {
                    break;
                }
                for bio in batch {
                    bio.complete(Err(SystemError::ENODEV));
                }
            }
        }

        if let Some(token_map) = bio_token_map {
            let pending: Vec<BioContext> = token_map
                .inner
                .lock_irqsave()
                .drain()
                .map(|(_, v)| v)
                .collect();
            for ctx in pending {
                ctx.bio.complete(Err(SystemError::ENODEV));
            }
        }
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

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOBlkDriver> {
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

/// IO线程入口函数
fn bio_io_thread_loop(device_weak: Weak<VirtIOBlkDevice>) -> i32 {
    loop {
        let device = match device_weak.upgrade() {
            Some(dev) => dev,
            None => {
                // 设备已被销毁，线程退出
                break;
            }
        };

        let bio_queue: Option<Arc<BioQueue>> = {
            let inner = device.inner();
            inner.bio_queue.clone()
        };

        if let Some(bio_queue) = bio_queue {
            // 等待队列中有请求
            if let Err(e) = bio_queue.wait_for_work() {
                log::error!("virtio bio wait_for_work interrupted: {:?}", e);
                continue;
            }

            let mut processed = 0;

            // 批量提交新请求，遵守budget限制
            while processed < IO_BUDGET {
                let batch = bio_queue.drain_batch();
                if batch.is_empty() {
                    break; // 队列空了，退出
                }

                for bio in batch {
                    if let Err(e) = submit_bio_to_virtio(&device, bio.clone()) {
                        log::error!("virtio submit_bio_to_virtio failed: {:?}", e);
                        // 失败时立即完成BIO
                        bio.complete(Err(e));
                    }
                    processed += 1;

                    if processed >= IO_BUDGET {
                        break; // 达到budget上限
                    }
                }
            }

            // 达到budget，主动睡眠20ms，避免独占CPU
            if processed >= IO_BUDGET {
                let sleep_time = PosixTimeSpec::new(0, (SLEEP_MS as i64) * 1_000_000); // 20ms
                let _ = nanosleep(sleep_time);
            }
        } else {
            break;
        }
    }

    0
}

/// 将BIO请求提交到VirtIO设备（异步）
fn submit_bio_to_virtio(
    device: &Arc<VirtIOBlkDevice>,
    bio: Arc<BioRequest>,
) -> Result<(), SystemError> {
    // 获取BIO信息
    let lba_start = bio.lba_start();
    let count = bio.count();
    let bio_type = bio.bio_type();

    // 获取token_map（clone以避免借用冲突）
    let token_map = {
        let inner = device.inner();
        inner
            .bio_token_map
            .as_ref()
            .ok_or(SystemError::EINVAL)?
            .clone()
    };

    // 创建请求和响应结构
    let mut req = Box::new(BlkReq::default());
    let mut resp = Box::new(BlkResp::default());

    // 获取buffer指针（在整个异步操作期间，bio会被BioContext持有，保证buffer有效）
    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

    // 提交异步请求，获取token
    let token = {
        let mut inner = device.inner();
        match bio_type {
            BioType::Read => {
                let buf_ptr = bio.buffer_mut();
                let buf = unsafe { &mut *buf_ptr };
                unsafe {
                    inner.device_inner.read_blocks_nb(
                        lba_start,
                        &mut req,
                        &mut buf[..count * LBA_SIZE],
                        &mut resp,
                    )
                }
                .map_err(|e| {
                    error!("VirtIOBlk async read_blocks_nb failed: {:?}", e);
                    SystemError::EIO
                })?
            }
            BioType::Write => {
                let buf_ptr = bio.buffer();
                let buf = unsafe { &*buf_ptr };
                unsafe {
                    inner.device_inner.write_blocks_nb(
                        lba_start,
                        &mut req,
                        &buf[..count * LBA_SIZE],
                        &mut resp,
                    )
                }
                .map_err(|e| {
                    error!("VirtIOBlk async write_blocks_nb failed: {:?}", e);
                    SystemError::EIO
                })?
            }
        }
    };

    // 保存上下文到token_map
    let ctx = BioContext {
        bio: bio.clone(),
        req,
        resp,
    };
    if token_map.insert(token, ctx).is_err() {
        drop(irq_guard);
        return Err(SystemError::EEXIST);
    }

    // 标记BIO为已提交
    if let Err(e) = bio.mark_submitted(token) {
        token_map.remove(token);
        drop(irq_guard);
        return Err(e);
    }

    drop(irq_guard);

    Ok(())
}
