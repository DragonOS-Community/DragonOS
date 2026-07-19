use core::{
    any::Any,
    fmt::{Debug, Formatter, Write},
    sync::atomic::{AtomicUsize, Ordering},
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
    arch::{CurrentIrqArch, CurrentTimeArch},
    driver::{
        base::{
            block::{
                bio::{BioRequest, BioType},
                bio_queue::{BioQueue, BioQueueWake},
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
            transport::{DeferredVirtioIrq, VirtIOTransport},
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
        vfs::{utils::DName, FilePrivateData, FileType, IndexNode, InodeMode, Metadata},
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
    time::{sleep::nanosleep, PosixTimeSpec, TimeArch},
};

const VIRTIO_BLK_BASENAME: &str = "virtio_blk";

// IO线程的budget配置
const IO_BUDGET: usize = 32; // 每次最多处理32个请求
const SLEEP_MS: usize = 20; // 达到budget后睡眠20ms
const SHUTDOWN_DRAIN_RETRIES: usize = 32;
const SHUTDOWN_DRAIN_INTERVAL_NS: i64 = 1_000_000;
const P6_2_STATS_ENABLED: bool = option_env!("DRAGONOS_P6_2_STATS").is_some();
const BIO_LATENCY_CYCLES_SHORT: usize = 10_000;
const BIO_LATENCY_CYCLES_MEDIUM: usize = 100_000;
const BIO_LATENCY_CYCLES_LONG: usize = 1_000_000;
static NEXT_VIRTIO_BLK_STATS_GENERATION: AtomicUsize = AtomicUsize::new(1);

#[derive(Default)]
struct InflightTiming {
    last_cycle: usize,
    inflight: usize,
    weighted_cycles: usize,
    observed_cycles: usize,
}

struct VirtIOBlkStats {
    generation: usize,
    submits: AtomicUsize,
    completes: AtomicUsize,
    reads: AtomicUsize,
    writes: AtomicUsize,
    flushes: AtomicUsize,
    bytes: AtomicUsize,
    errors: AtomicUsize,
    short: AtomicUsize,
    inflight: AtomicUsize,
    peak_inflight: AtomicUsize,
    depth_1: AtomicUsize,
    depth_2_4: AtomicUsize,
    depth_5_16: AtomicUsize,
    depth_17_plus: AtomicUsize,
    size_4k: AtomicUsize,
    size_16k: AtomicUsize,
    size_32k: AtomicUsize,
    size_64k: AtomicUsize,
    size_large: AtomicUsize,
    budget_hits: AtomicUsize,
    latency_short: AtomicUsize,
    latency_medium: AtomicUsize,
    latency_long: AtomicUsize,
    latency_very_long: AtomicUsize,
    timing: SpinLock<InflightTiming>,
}

impl VirtIOBlkStats {
    fn new() -> Self {
        Self {
            generation: NEXT_VIRTIO_BLK_STATS_GENERATION.fetch_add(1, Ordering::Relaxed),
            submits: AtomicUsize::new(0),
            completes: AtomicUsize::new(0),
            reads: AtomicUsize::new(0),
            writes: AtomicUsize::new(0),
            flushes: AtomicUsize::new(0),
            bytes: AtomicUsize::new(0),
            errors: AtomicUsize::new(0),
            short: AtomicUsize::new(0),
            inflight: AtomicUsize::new(0),
            peak_inflight: AtomicUsize::new(0),
            depth_1: AtomicUsize::new(0),
            depth_2_4: AtomicUsize::new(0),
            depth_5_16: AtomicUsize::new(0),
            depth_17_plus: AtomicUsize::new(0),
            size_4k: AtomicUsize::new(0),
            size_16k: AtomicUsize::new(0),
            size_32k: AtomicUsize::new(0),
            size_64k: AtomicUsize::new(0),
            size_large: AtomicUsize::new(0),
            budget_hits: AtomicUsize::new(0),
            latency_short: AtomicUsize::new(0),
            latency_medium: AtomicUsize::new(0),
            latency_long: AtomicUsize::new(0),
            latency_very_long: AtomicUsize::new(0),
            timing: SpinLock::new(InflightTiming::default()),
        }
    }

    fn account_time(&self, now: usize, inflight_delta: isize) {
        let mut timing = self.timing.lock_irqsave();
        if timing.last_cycle != 0 {
            let elapsed = now.wrapping_sub(timing.last_cycle);
            timing.observed_cycles = timing.observed_cycles.wrapping_add(elapsed);
            timing.weighted_cycles = timing
                .weighted_cycles
                .wrapping_add(elapsed.wrapping_mul(timing.inflight));
        }
        timing.last_cycle = now;
        if inflight_delta > 0 {
            timing.inflight += inflight_delta as usize;
        } else {
            timing.inflight = timing.inflight.saturating_sub((-inflight_delta) as usize);
        }
    }

    fn begin(&self, kind: BioType, bytes: usize, now: usize) {
        if !P6_2_STATS_ENABLED {
            return;
        }
        self.submits.fetch_add(1, Ordering::Relaxed);
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
        match kind {
            BioType::Read => &self.reads,
            BioType::Write => &self.writes,
            BioType::Flush => &self.flushes,
        }
        .fetch_add(1, Ordering::Relaxed);
        if kind != BioType::Flush {
            match bytes {
                1..=4096 => &self.size_4k,
                4097..=16384 => &self.size_16k,
                16385..=32768 => &self.size_32k,
                32769..=65536 => &self.size_64k,
                _ => &self.size_large,
            }
            .fetch_add(1, Ordering::Relaxed);
        }
        let depth = self.inflight.fetch_add(1, Ordering::Relaxed) + 1;
        self.peak_inflight.fetch_max(depth, Ordering::Relaxed);
        match depth {
            1 => &self.depth_1,
            2..=4 => &self.depth_2_4,
            5..=16 => &self.depth_5_16,
            _ => &self.depth_17_plus,
        }
        .fetch_add(1, Ordering::Relaxed);
        self.account_time(now, 1);
    }

    fn finish(
        &self,
        result: &Result<usize, SystemError>,
        expected: usize,
        start: usize,
        now: usize,
    ) {
        if !P6_2_STATS_ENABLED {
            return;
        }
        self.completes.fetch_add(1, Ordering::Relaxed);
        let previous = self.inflight.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(previous != 0);
        match result {
            Ok(completed) if *completed != expected => {
                self.short.fetch_add(1, Ordering::Relaxed);
            }
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
            _ => {}
        }
        let latency = now.wrapping_sub(start);
        let bucket = if latency <= BIO_LATENCY_CYCLES_SHORT {
            &self.latency_short
        } else if latency <= BIO_LATENCY_CYCLES_MEDIUM {
            &self.latency_medium
        } else if latency <= BIO_LATENCY_CYCLES_LONG {
            &self.latency_long
        } else {
            &self.latency_very_long
        };
        bucket.fetch_add(1, Ordering::Relaxed);
        self.account_time(now, -1);
    }
}

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

    pub fn drain_all(&self) -> Vec<BioContext> {
        self.inner
            .lock_irqsave()
            .drain()
            .map(|(_, ctx)| ctx)
            .collect()
    }

    pub fn is_empty(&self) -> bool {
        self.inner.lock_irqsave().is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum VirtIOBlkState {
    Online,
    Quiescing,
    Draining,
    Reset,
    Dead,
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
        complete_used_requests(&device, &self.token_map);
    }
}

fn complete_used_requests(device: &Arc<VirtIOBlkDevice>, token_map: &Arc<BioTokenMap>) -> usize {
    let mut completed = 0;
    loop {
        let mut inner = device.inner();
        if inner.state >= VirtIOBlkState::Reset {
            break;
        }

        let device_inner = match inner.device_inner.as_mut() {
            Some(device_inner) => device_inner,
            None => break,
        };

        let token = match device_inner.peek_used() {
            Some(token) => token,
            None => break,
        };

        let ctx = match token_map.remove(token) {
            Some(ctx) => ctx,
            None => {
                error!("VirtIOBlk: token {} not found in token_map", token);
                break;
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
                    device_inner.complete_read_blocks(
                        token,
                        &ctx.req,
                        &mut buf[..count * LBA_SIZE],
                        &mut ctx.resp,
                    )
                } {
                    Ok(_) => Ok(count * LBA_SIZE),
                    Err(VirtioError::NotReady) => {
                        if token_map.insert(token, ctx).is_err() {
                            error!("VirtIOBlk: token {} reinsert failed", token);
                            device.complete_accounted_bio(&bio, Err(SystemError::EIO));
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
                    device_inner.complete_write_blocks(
                        token,
                        &ctx.req,
                        &buf[..count * LBA_SIZE],
                        &mut ctx.resp,
                    )
                } {
                    Ok(_) => Ok(count * LBA_SIZE),
                    Err(VirtioError::NotReady) => {
                        if token_map.insert(token, ctx).is_err() {
                            error!("VirtIOBlk: token {} reinsert failed", token);
                            device.complete_accounted_bio(&bio, Err(SystemError::EIO));
                        }
                        break;
                    }
                    Err(e) => {
                        error!("VirtIOBlk complete_write_blocks failed: {:?}", e);
                        Err(SystemError::EIO)
                    }
                }
            }
            BioType::Flush => {
                match unsafe { device_inner.complete_flush(token, &ctx.req, &mut ctx.resp) } {
                    Ok(_) => Ok(0),
                    Err(VirtioError::NotReady) => {
                        if token_map.insert(token, ctx).is_err() {
                            error!("VirtIOBlk: token {} reinsert failed", token);
                            device.complete_accounted_bio(&bio, Err(SystemError::EIO));
                        }
                        break;
                    }
                    Err(e) => {
                        error!("VirtIOBlk complete_flush failed: {:?}", e);
                        Err(SystemError::EIO)
                    }
                }
            }
        };
        drop(inner);
        device.complete_accounted_bio(&bio, result);
        completed += 1;
    }

    completed
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

pub fn virtio_blk_stats_report() -> String {
    let mut report = String::new();
    for device in virtio_blk_driver().devices() {
        let Ok(device) = device.arc_any().downcast::<VirtIOBlkDevice>() else {
            continue;
        };
        let stats = &device.stats;
        let timing = stats.timing.lock_irqsave();
        let _ = writeln!(
            report,
            "device={} generation={} enabled={} submits={} completes={} reads={} writes={} flushes={} bytes={} errors={} short={} inflight={} peak_inflight={} depth_1={} depth_2_4={} depth_5_16={} depth_17_plus={} size_4k={} size_16k={} size_32k={} size_64k={} size_large={} budget_hits={} latency_le_10k={} latency_le_100k={} latency_le_1m={} latency_gt_1m={} weighted_inflight_cycles={} observed_cycles={}",
            device.blkdev_meta.devname.name(),
            stats.generation,
            P6_2_STATS_ENABLED as usize,
            stats.submits.load(Ordering::Relaxed),
            stats.completes.load(Ordering::Relaxed),
            stats.reads.load(Ordering::Relaxed),
            stats.writes.load(Ordering::Relaxed),
            stats.flushes.load(Ordering::Relaxed),
            stats.bytes.load(Ordering::Relaxed),
            stats.errors.load(Ordering::Relaxed),
            stats.short.load(Ordering::Relaxed),
            stats.inflight.load(Ordering::Relaxed),
            stats.peak_inflight.load(Ordering::Relaxed),
            stats.depth_1.load(Ordering::Relaxed),
            stats.depth_2_4.load(Ordering::Relaxed),
            stats.depth_5_16.load(Ordering::Relaxed),
            stats.depth_17_plus.load(Ordering::Relaxed),
            stats.size_4k.load(Ordering::Relaxed),
            stats.size_16k.load(Ordering::Relaxed),
            stats.size_32k.load(Ordering::Relaxed),
            stats.size_64k.load(Ordering::Relaxed),
            stats.size_large.load(Ordering::Relaxed),
            stats.budget_hits.load(Ordering::Relaxed),
            stats.latency_short.load(Ordering::Relaxed),
            stats.latency_medium.load(Ordering::Relaxed),
            stats.latency_long.load(Ordering::Relaxed),
            stats.latency_very_long.load(Ordering::Relaxed),
            timing.weighted_cycles,
            timing.observed_cycles,
        );
    }
    report
}

pub fn virtio_blk(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    let device = VirtIOBlkDevice::new(transport, dev_id);
    if let Some((device, deferred_irq)) = device {
        if let Some(dev_parent) = dev_parent {
            device.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
        }
        if let Some(deferred_irq) = deferred_irq {
            if let Err(err) = deferred_irq.install(device.dev_id().clone()) {
                error!(
                    "VirtIOBlkDevice '{:?}' setup_irq failed: {:?}",
                    device.dev_id(),
                    err
                );
                device.abort_initialization();
                return;
            }
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
#[cast_to([sync] Device, DeviceINode)]
pub struct VirtIOBlkDevice {
    blkdev_meta: BlockDevMeta,
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIOBlkDevice>,
    locked_kobj_state: LockedKObjectState,
    self_ref: Weak<Self>,
    parent: RwLock<Weak<LockedDevFSInode>>,
    fs: RwLock<Weak<DevFS>>,
    metadata: Metadata,
    stats: VirtIOBlkStats,
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
    pub(crate) fn new(
        transport: VirtIOTransport,
        dev_id: Arc<DeviceId>,
    ) -> Option<(Arc<Self>, Option<DeferredVirtioIrq>)> {
        // 设置中断
        let irq_setup = match transport.setup_irq(dev_id.clone()) {
            Ok(setup) => setup,
            Err(err) => {
                error!("VirtIOBlkDevice '{dev_id:?}' setup_irq failed: {:?}", err);
                return None;
            }
        };

        let devname = virtioblk_manager().alloc_id()?;
        let irq = Some(transport.irq());
        let irq_is_msix = transport.irq_is_msix();
        let device_inner = VirtIOBlk::<HalImpl, VirtIOTransport>::new(transport);
        if let Err(e) = device_inner {
            error!("VirtIOBlkDevice '{dev_id:?}' create failed: {:?}", e);
            return None;
        }

        let mut device_inner: VirtIOBlk<HalImpl, VirtIOTransport> = device_inner.unwrap();
        device_inner.enable_interrupts();
        let capacity_blocks = device_inner.capacity() as usize * SECTOR_SIZE / LBA_SIZE;

        // 创建BIO队列和token映射表
        let bio_queue = BioQueue::new();
        let bio_token_map = BioTokenMap::new();

        let dev = Arc::new_cyclic(|self_ref| Self {
            blkdev_meta: BlockDevMeta::new(devname.clone(), Major::VIRTIO_BLK_MAJOR),
            self_ref: self_ref.clone(),
            dev_id,
            locked_kobj_state: LockedKObjectState::default(),
            inner: SpinLock::new(InnerVirtIOBlkDevice {
                device_inner: Some(device_inner),
                state: VirtIOBlkState::Online,
                capacity_blocks,
                name: None,
                virtio_index: None,
                device_common: DeviceCommonData::default(),
                kobject_common: KObjectCommonData::default(),
                irq,
                irq_is_msix,
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
            stats: VirtIOBlkStats::new(),
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

        Some((dev, irq_setup.into_deferred()))
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOBlkDevice> {
        self.inner.lock_irqsave()
    }

    fn expected_bio_bytes(bio: &BioRequest) -> usize {
        match bio.bio_type() {
            BioType::Read | BioType::Write => bio.count() * LBA_SIZE,
            BioType::Flush => 0,
        }
    }

    fn complete_accounted_bio(&self, bio: &BioRequest, result: Result<usize, SystemError>) {
        if P6_2_STATS_ENABLED {
            self.stats.finish(
                &result,
                Self::expected_bio_bytes(bio),
                bio.stats_submit_cycle(),
                CurrentTimeArch::get_cycles(),
            );
        }
        bio.complete(result);
    }

    fn abort_initialization(self: &Arc<Self>) {
        let devname_id = self.blkdev_meta.devname.id();
        if let Err(err) = self.shutdown() {
            error!(
                "VirtIOBlkDevice '{:?}' initialization cleanup failed: {:?}",
                self.dev_id, err
            );
            return;
        }
        virtioblk_manager().free_id(devname_id);
    }

    fn shutdown(self: &Arc<Self>) -> Result<(), SystemError> {
        let (bio_queue, bio_token_map, io_thread_pcb) = {
            let mut inner = self.inner();
            if inner.state >= VirtIOBlkState::Quiescing {
                return Ok(());
            }

            inner.state = VirtIOBlkState::Quiescing;
            if let Some(device_inner) = inner.device_inner.as_mut() {
                device_inner.disable_interrupts();
            }

            (
                inner.bio_queue.clone(),
                inner.bio_token_map.clone(),
                inner.io_thread_pcb.clone(),
            )
        };

        if let Some(bio_queue) = &bio_queue {
            bio_queue.begin_quiesce();
        }

        if let Some(bio_queue) = &bio_queue {
            for bio in bio_queue.stop_and_drain() {
                self.complete_accounted_bio(&bio, Err(SystemError::ESHUTDOWN));
            }
        }

        if let Some(io_thread_pcb) = &io_thread_pcb {
            KernelThreadMechanism::stop(io_thread_pcb)?;
        }

        {
            let mut inner = self.inner();
            if inner.state < VirtIOBlkState::Draining {
                inner.state = VirtIOBlkState::Draining;
            }
        }

        if let Some(bio_token_map) = &bio_token_map {
            for _ in 0..SHUTDOWN_DRAIN_RETRIES {
                complete_used_requests(self, bio_token_map);
                if bio_token_map.is_empty() {
                    break;
                }

                let sleep_time = PosixTimeSpec::new(0, SHUTDOWN_DRAIN_INTERVAL_NS);
                let _ = nanosleep(sleep_time);
            }
        }

        let device_inner = {
            let mut inner = self.inner();
            inner.state = VirtIOBlkState::Reset;
            inner.device_inner.take()
        };
        drop(device_inner);

        if let Some(bio_token_map) = &bio_token_map {
            let pending = bio_token_map.drain_all();
            if !pending.is_empty() {
                log::warn!(
                    "VirtIOBlkDevice '{:?}' reset with {} in-flight BIO(s)",
                    self.dev_id.id(),
                    pending.len()
                );
            }
            for ctx in pending {
                self.complete_accounted_bio(&ctx.bio, Err(SystemError::ESHUTDOWN));
            }
        }

        let mut inner = self.inner();
        inner.state = VirtIOBlkState::Dead;
        inner.bio_queue = None;
        inner.bio_token_map = None;
        inner.io_thread_pcb = None;
        inner.completion_tasklet = None;

        Ok(())
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

    fn supports_post_write_sync(&self, file_type: FileType) -> bool {
        file_type == FileType::BlockDevice
    }

    fn sync_file(
        &self,
        _datasync: bool,
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        <Self as BlockDevice>::sync(self)
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
        let blocks = inner.capacity_blocks;
        drop(inner);
        GeneralBlockRange::new(0, blocks).unwrap()
    }

    fn read_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &mut [u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }
        let expected = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;
        if buf.len() < expected {
            return Err(SystemError::EINVAL);
        }
        let bio = self.submit_bio_read(lba_id_start, count)?;
        let data = bio
            .wait()
            .inspect_err(|e| log::error!("VirtIOBlkDevice read_at_sync error: {:?}", e))?;
        buf[..expected].copy_from_slice(&data);
        Ok(expected)
    }

    fn write_at_sync(
        &self,
        lba_id_start: BlockId,
        count: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        if count == 0 {
            return Ok(0);
        }
        let expected = count.checked_mul(LBA_SIZE).ok_or(SystemError::EOVERFLOW)?;
        if buf.len() != expected {
            return Err(SystemError::EINVAL);
        }
        let bio = self.submit_bio_write(lba_id_start, count, buf)?;
        bio.wait_status()
            .inspect_err(|e| log::error!("VirtIOBlkDevice write_at_sync error: {:?}", e))?;
        Ok(expected)
    }

    fn sync(&self) -> Result<(), SystemError> {
        let bio = BioRequest::new_flush();
        self.submit_bio(bio.clone())?;
        bio.wait_status()
            .inspect_err(|e| log::error!("VirtIOBlkDevice sync flush error: {:?}", e))?;
        Ok(())
    }

    fn supports_reliable_flush(&self) -> bool {
        self.inner()
            .device_inner
            .as_ref()
            .is_some_and(|device| device.supports_flush())
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
        if inner.state != VirtIOBlkState::Online {
            return Err(SystemError::ESHUTDOWN);
        }
        if let Some(bio_queue) = &inner.bio_queue {
            let (expected, now) = if P6_2_STATS_ENABLED {
                let expected = match bio.bio_type() {
                    BioType::Read | BioType::Write => bio
                        .count()
                        .checked_mul(LBA_SIZE)
                        .ok_or(SystemError::EOVERFLOW)?,
                    BioType::Flush => 0,
                };
                let now = CurrentTimeArch::get_cycles();
                bio.set_stats_submit_cycle(now);
                self.stats.begin(bio.bio_type(), expected, now);
                (expected, now)
            } else {
                (0, 0)
            };
            if let Err(error) = bio_queue.submit(bio) {
                if P6_2_STATS_ENABLED {
                    self.stats.finish(
                        &Err(error.clone()),
                        expected,
                        now,
                        CurrentTimeArch::get_cycles(),
                    );
                }
                return Err(error);
            }
            Ok(())
        } else {
            Err(SystemError::ENOSYS)
        }
    }
}

struct InnerVirtIOBlkDevice {
    device_inner: Option<VirtIOBlk<HalImpl, VirtIOTransport>>,
    state: VirtIOBlkState,
    capacity_blocks: usize,
    name: Option<String>,
    virtio_index: Option<VirtIODeviceIndex>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    irq: Option<IrqNumber>,
    irq_is_msix: bool,
    bio_queue: Option<Arc<BioQueue>>,
    bio_token_map: Option<Arc<BioTokenMap>>,
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
        if inner.state >= VirtIOBlkState::Reset {
            return Ok(crate::exception::irqdesc::IrqReturn::NotHandled);
        }

        let device_inner = match inner.device_inner.as_mut() {
            Some(device_inner) => device_inner,
            None => return Ok(crate::exception::irqdesc::IrqReturn::NotHandled),
        };

        let acked = device_inner.ack_interrupt();
        if !acked && !inner.irq_is_msix {
            log::debug!(
                "VirtIOBlkDevice '{:?}' ack_interrupt not set",
                self.dev_id.id()
            );
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
        self.inner().state == VirtIOBlkState::Dead
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
                    self.complete_accounted_bio(&bio, Err(SystemError::ENODEV));
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
                self.complete_accounted_bio(&ctx.bio, Err(SystemError::ENODEV));
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

    fn shutdown(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let dev = device
            .clone()
            .arc_any()
            .downcast::<VirtIOBlkDevice>()
            .map_err(|_| {
                error!(
                "VirtIOBlkDriver::shutdown() failed: device is not a VirtIO block device. Device: '{:?}'",
                device.name()
            );
                SystemError::EINVAL
            })?;

        dev.shutdown()
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
            let stopping = match bio_queue.wait_for_work_or_stop() {
                BioQueueWake::WorkAvailable => false,
                BioQueueWake::Stopping => true,
            };

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
                        device.complete_accounted_bio(&bio, Err(e));
                    }
                    processed += 1;

                    if processed >= IO_BUDGET {
                        break; // 达到budget上限
                    }
                }
            }

            // 达到budget，主动睡眠20ms，避免独占CPU
            if processed >= IO_BUDGET {
                if P6_2_STATS_ENABLED {
                    device.stats.budget_hits.fetch_add(1, Ordering::Relaxed);
                }
                let sleep_time = PosixTimeSpec::new(0, (SLEEP_MS as i64) * 1_000_000); // 20ms
                let _ = nanosleep(sleep_time);
            }

            if stopping {
                break;
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
        if inner.state != VirtIOBlkState::Online {
            return Err(SystemError::ESHUTDOWN);
        }
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
        if inner.state != VirtIOBlkState::Online {
            return Err(SystemError::ESHUTDOWN);
        }

        let device_inner = inner.device_inner.as_mut().ok_or(SystemError::ENODEV)?;
        match bio_type {
            BioType::Read => {
                let buf_ptr = bio.buffer_mut();
                let buf = unsafe { &mut *buf_ptr };
                Some(
                    unsafe {
                        device_inner.read_blocks_nb(
                            lba_start,
                            &mut req,
                            &mut buf[..count * LBA_SIZE],
                            &mut resp,
                        )
                    }
                    .map_err(|e| {
                        error!("VirtIOBlk async read_blocks_nb failed: {:?}", e);
                        SystemError::EIO
                    })?,
                )
            }
            BioType::Write => {
                let buf_ptr = bio.buffer();
                let buf = unsafe { &*buf_ptr };
                Some(
                    unsafe {
                        device_inner.write_blocks_nb(
                            lba_start,
                            &mut req,
                            &buf[..count * LBA_SIZE],
                            &mut resp,
                        )
                    }
                    .map_err(|e| {
                        error!("VirtIOBlk async write_blocks_nb failed: {:?}", e);
                        SystemError::EIO
                    })?,
                )
            }
            BioType::Flush => {
                unsafe { device_inner.flush_nb(&mut req, &mut resp) }.map_err(|e| {
                    error!("VirtIOBlk async flush_nb failed: {:?}", e);
                    SystemError::EIO
                })?
            }
        }
    };

    let token = match token {
        Some(token) => token,
        None => {
            device.complete_accounted_bio(&bio, Ok(0));
            drop(irq_guard);
            return Ok(());
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
