//! VirtIO balloon page ownership and free-page reporting.
//!
//! One permanent task owns all queues. Each queue progresses independently:
//! delayed reporting must never prevent an OOM deflate request. Neither an
//! elapsed deadline nor an invalid completion transfers ownership back from
//! the device; only a valid used entry or a confirmed reset does so.
use alloc::{boxed::Box, string::ToString, sync::Arc};
use core::{
    mem, slice,
    sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};
use system_error::SystemError;
use virtio_drivers::{
    queue::VirtQueue,
    transport::{DeviceStatus, Transport},
};

use super::{
    irq::{virtio_irq_manager, VirtioIrqCallback},
    transport::{PciVirtioIrqLease, VirtIOTransport},
    virtio_impl::VirtioQueueHal,
};
use crate::{
    arch::{mm::LockedFrameAllocator, MMArch},
    driver::base::device::DeviceId,
    exception::{irqdesc::IrqReturn, IrqNumber},
    libs::{
        mutex::Mutex,
        spinlock::SpinLock,
        wait_queue::{Waiter, Waker},
    },
    mm::{
        allocator::page_frame::{
            allocate_page_frames_request, deallocate_page_frames, FrameAllocationRequest,
            PageFrameCount, PhysPageFrame,
        },
        page_reporting::{self, ReportingBlock},
        MemoryManagementArch, PhysAddr,
    },
    process::{
        kthread::{KernelThreadClosure, KernelThreadMechanism},
        ProcessControlBlock, ProcessManager,
    },
    time::timer::{clock, next_n_us_timer_jiffies},
};

const BATCH_PAGES: usize = 256;
const QUEUE_SIZE: usize = 8;
const MAX_PFN_ADDRESS: usize = ((u32::MAX as u64 + 1) * 4096 - 1) as usize;
const POLL_US: u64 = 10_000;
const IDLE_US: u64 = 1_000_000;
const OOM_TIMEOUT_US: u64 = 1_000_000;
const REQUEST_TIMEOUT_US: u64 = 5_000_000;
const OOM_COOLDOWN_US: u64 = 5_000_000;
type Queue = VirtQueue<VirtioQueueHal, QUEUE_SIZE>;

bitflags2::bitflags! {
    #[derive(Debug, Clone, Copy)]
    struct Features: u64 {
        const MUST_TELL_HOST = 1 << 0;
        const DEFLATE_ON_OOM = 1 << 2;
        const REPORTING = 1 << 5;
        const VERSION_1 = 1 << 32;
    }
}

/// Metadata is separate from the physical pages the host may discard.
#[repr(C, align(4096))]
struct Batch {
    pfns: [u32; BATCH_PAGES],
    count: usize,
    next: Option<Box<Batch>>,
}
impl Batch {
    fn try_new() -> Option<Box<Self>> {
        // A page-aligned layout goes through the existing direct-map buddy
        // buffer allocator's no-retry path. Never recurse into OOM from the
        // very worker that must service an outstanding deflate request.
        let pointer = unsafe {
            crate::KERNEL_ALLOCATOR.alloc_buffer_zeroed(core::alloc::Layout::new::<Self>())
        };
        if pointer.is_null() {
            return None;
        }
        unsafe { Some(Box::from_raw(pointer.cast::<Self>())) }
    }

    fn bytes(&self, count: usize) -> &[u8] {
        // PFNs are wire little endian and the array is physically contiguous.
        unsafe {
            slice::from_raw_parts(self.pfns.as_ptr().add(self.count - count).cast(), count * 4)
        }
    }
    fn release_tail(&mut self, count: usize) {
        assert!(count <= self.count);
        for pfn in &self.pfns[self.count - count..self.count] {
            // Only called after completion/reset, or before submission.
            unsafe {
                deallocate_page_frames(
                    PhysPageFrame::new(PhysAddr::new((u32::from_le(*pfn) as usize) << 12)),
                    PageFrameCount::ONE,
                );
            }
        }
        self.count -= count;
    }
}
impl Drop for Batch {
    fn drop(&mut self) {
        self.release_tail(self.count);
        // Large balloons must not recursively destroy their metadata chain.
        let mut next = self.next.take();
        while let Some(mut batch) = next {
            next = batch.next.take();
        }
    }
}

struct PfnRequest {
    token: u16,
    deadline: u64,
    batch: Box<Batch>,
    count: usize,
}
struct ReportRequest {
    token: u16,
    deadline: u64,
    block: ReportingBlock,
}

struct Device {
    transport: Arc<SpinLock<VirtIOTransport>>,
    features: Features,
    inflate: Option<Queue>,
    deflate: Option<Queue>,
    reporting: Option<Queue>,
    reporting_index: u16,
    inflating: Option<PfnRequest>,
    deflating: Option<PfnRequest>,
    report: Option<ReportRequest>,
    pages: Option<Box<Batch>>,
    actual: usize,
    oom_seen: u64,
    cooldown: u64,
    failed: bool,
    reported: u64,
    // The IRQ lease and callback are pinned for the device's permanent life.
    _irq_lease: Option<PciVirtioIrqLease>,
}
// All queues and page owners are exclusively accessed by the permanent worker.
unsafe impl Send for Device {}

struct Balloon {
    device: Mutex<Device>,
    next: Option<Arc<Balloon>>,
}
struct Interrupt {
    transport: Arc<SpinLock<VirtIOTransport>>,
    dedicated: bool,
}
// SAFETY: the transport mappings remain pinned for this permanent device;
// every register access is serialized by the same IRQ-saving transport lock.
unsafe impl Send for Interrupt {}
unsafe impl Sync for Interrupt {}
impl VirtioIrqCallback for Interrupt {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError> {
        // MSI/MSI-X delivery identifies this device even when the legacy
        // ISR status is zero. Shared MMIO IRQs still require an asserted ISR.
        let acked = self.transport.lock_irqsave().ack_interrupt() || self.dedicated;
        if acked {
            request_work();
        }
        Ok(if acked {
            IrqReturn::Handled
        } else {
            IrqReturn::NotHandled
        })
    }
}

static DEVICES: SpinLock<Option<Arc<Balloon>>> = SpinLock::new(None);
static WORKER: SpinLock<Option<Arc<ProcessControlBlock>>> = SpinLock::new(None);
static WAKER: SpinLock<Option<Arc<Waker>>> = SpinLock::new(None);
static DEADLINE: AtomicU64 = AtomicU64::new(0);
static WORK_PENDING: AtomicBool = AtomicBool::new(false);
static OOM_REQUEST: AtomicU64 = AtomicU64::new(0);
static OOM_WAITING: AtomicBool = AtomicBool::new(false);
static RECLAIMABLE: AtomicUsize = AtomicUsize::new(0);
static FREED: AtomicU64 = AtomicU64::new(0);

fn request_work() {
    WORK_PENDING.store(true, Ordering::Release);
    crate::mm::memory_notify::request_balloon_deadline();
}

/// Tick never calls a scheduler or acquires a device/queue/page lock.
pub(crate) fn tick(now: u64) {
    let deadline = DEADLINE.load(Ordering::Acquire);
    if deadline != 0 && now >= deadline {
        request_work();
    }
}

/// Only called in memory-notifier task context. Both permanent references
/// remain pinned, so a transient callback can never retire the worker PCB.
pub(crate) fn notify_deadline() {
    let waker = WAKER.lock_irqsave().as_ref().cloned();
    if let Some(waker) = waker {
        waker.wake();
    }
    if OOM_WAITING.load(Ordering::Acquire) {
        crate::libs::resource_wait::wake_all();
    }
}

/// Global OOM selection serializes callers. Must run before victim/work locks.
/// Request metadata and the resource wait link are already embedded in globals
/// and the caller PCB. A late host completion remains safe after the timeout.
pub(crate) fn deflate_on_oom() -> bool {
    if RECLAIMABLE.load(Ordering::Acquire) == 0 {
        return false;
    }
    let before = FREED.load(Ordering::Acquire);
    let deadline = next_n_us_timer_jiffies(OOM_TIMEOUT_US);
    OOM_WAITING.store(true, Ordering::Release);
    OOM_REQUEST.fetch_add(1, Ordering::AcqRel);
    request_work();
    let result = crate::libs::resource_wait::wait_until_killable(|| {
        if FREED.load(Ordering::Acquire) != before {
            Some(true)
        } else if clock() >= deadline {
            Some(false)
        } else {
            None
        }
    })
    .unwrap_or(false);
    OOM_WAITING.store(false, Ordering::Release);
    result
}

impl Device {
    fn reclaimable(&self) -> bool {
        self.features.contains(Features::DEFLATE_ON_OOM)
    }

    fn change_actual(&mut self, count: usize, add: bool) {
        let reclaimable = self.reclaimable();
        if add {
            self.actual += count;
            crate::mm::balloon::add_pages(count, reclaimable);
            if reclaimable {
                RECLAIMABLE.fetch_add(count, Ordering::Release);
            }
        } else {
            self.actual -= count;
            crate::mm::balloon::remove_pages(count, reclaimable);
            if reclaimable {
                RECLAIMABLE.fetch_sub(count, Ordering::Release);
            }
        }
        if let Ok(config) = self.transport.lock_irqsave().config_space::<[u32; 2]>() {
            // Balloon config fields are LE even for legacy transports.
            unsafe {
                core::ptr::addr_of_mut!((*config.as_ptr())[1])
                    .write_volatile((self.actual as u32).to_le());
            }
        }
    }

    fn target(&self) -> Result<usize, virtio_drivers::Error> {
        let transport = self.transport.lock_irqsave();
        if transport
            .get_status()
            .contains(DeviceStatus::DEVICE_NEEDS_RESET)
        {
            return Err(virtio_drivers::Error::IoError);
        }
        let config = transport.config_space::<[u32; 2]>()?;
        Ok(
            u32::from_le(unsafe { core::ptr::addr_of!((*config.as_ptr())[0]).read_volatile() })
                as usize,
        )
    }

    fn complete(&mut self) -> Result<(), virtio_drivers::Error> {
        if self.inflate.as_ref().is_some_and(Queue::can_pop) {
            let request = self
                .inflating
                .as_ref()
                .ok_or(virtio_drivers::Error::WrongToken)?;
            let buffer = request.batch.bytes(request.count);
            // Stable Batch allocation stays device-owned on any error.
            unsafe {
                self.inflate
                    .as_mut()
                    .unwrap()
                    .pop_used(request.token, &[buffer], &mut [])?;
            }
            let mut request = self.inflating.take().unwrap();
            self.change_actual(request.count, true);
            request.batch.next = self.pages.take();
            self.pages = Some(request.batch);
        }
        if self.deflate.as_ref().is_some_and(Queue::can_pop) {
            let request = self
                .deflating
                .as_ref()
                .ok_or(virtio_drivers::Error::WrongToken)?;
            let buffer = request.batch.bytes(request.count);
            unsafe {
                self.deflate
                    .as_mut()
                    .unwrap()
                    .pop_used(request.token, &[buffer], &mut [])?;
            }
            let mut request = self.deflating.take().unwrap();
            self.change_actual(request.count, false);
            request.batch.release_tail(request.count);
            FREED.fetch_add(request.count as u64, Ordering::Release);
            if request.batch.count != 0 {
                request.batch.next = self.pages.take();
                self.pages = Some(request.batch);
            }
        }
        if self.reporting.as_ref().is_some_and(Queue::can_pop) {
            let request = self
                .report
                .as_mut()
                .ok_or(virtio_drivers::Error::WrongToken)?;
            let buffer = report_buffer(&mut request.block);
            // REPORTING is device-writable; content is never inspected.
            unsafe {
                self.reporting
                    .as_mut()
                    .unwrap()
                    .pop_used(request.token, &[], &mut [buffer])?;
            }
            let request = self.report.take().unwrap();
            page_reporting::finish(request.block, true);
            self.reported += 1;
        }
        Ok(())
    }

    fn submit_inflate(&mut self, count: usize) -> Result<bool, virtio_drivers::Error> {
        let Some(mut batch) = Batch::try_new() else {
            return Ok(false);
        };
        for _ in 0..count.min(BATCH_PAGES) {
            // User floor, bounded PFN, no reclaim recursion or OOM selection.
            let Some((address, _)) = (unsafe {
                allocate_page_frames_request(FrameAllocationRequest::user(
                    PageFrameCount::ONE,
                    PhysAddr::new(MAX_PFN_ADDRESS),
                ))
            }) else {
                break;
            };
            batch.pfns[batch.count] = ((address.data() >> 12) as u32).to_le();
            batch.count += 1;
        }
        if batch.count == 0 {
            return Ok(false);
        }
        let count = batch.count;
        let token = unsafe {
            self.inflate
                .as_mut()
                .unwrap()
                .add(&[batch.bytes(count)], &mut [])?
        };
        self.inflating = Some(PfnRequest {
            token,
            deadline: next_n_us_timer_jiffies(REQUEST_TIMEOUT_US),
            batch,
            count,
        });
        self.notify(0);
        Ok(true)
    }

    fn submit_deflate(&mut self, count: usize) -> Result<bool, virtio_drivers::Error> {
        let Some(mut batch) = self.pages.take() else {
            return Ok(false);
        };
        self.pages = batch.next.take();
        let count = count.min(batch.count);
        let result = unsafe {
            self.deflate
                .as_mut()
                .unwrap()
                .add(&[batch.bytes(count)], &mut [])
        };
        match result {
            Ok(token) => {
                self.deflating = Some(PfnRequest {
                    token,
                    deadline: next_n_us_timer_jiffies(REQUEST_TIMEOUT_US),
                    batch,
                    count,
                });
                self.notify(1);
                Ok(true)
            }
            Err(error) => {
                batch.next = self.pages.take();
                self.pages = Some(batch);
                Err(error)
            }
        }
    }

    fn submit_report(&mut self) -> Result<(), virtio_drivers::Error> {
        let Some(mut block) = page_reporting::try_take() else {
            return Ok(());
        };
        let buffer = report_buffer(&mut block);
        let result = unsafe { self.reporting.as_mut().unwrap().add(&[], &mut [buffer]) };
        match result {
            Ok(token) => {
                self.report = Some(ReportRequest {
                    token,
                    deadline: next_n_us_timer_jiffies(REQUEST_TIMEOUT_US),
                    block,
                });
                self.notify(self.reporting_index);
            }
            Err(error) => {
                page_reporting::finish(block, false);
                return Err(error);
            }
        }
        Ok(())
    }

    fn notify(&mut self, queue: u16) {
        // No EVENT_IDX was negotiated, so notification suppression is the
        // normal avail-ring flag. The host is always allowed an early kick.
        self.transport.lock_irqsave().notify(queue);
    }

    fn step(&mut self) -> Result<bool, virtio_drivers::Error> {
        if self.failed {
            return Ok(false);
        }
        // Required in polling mode as config interrupts are independent of
        // the avail-ring NO_INTERRUPT hint. Also harmless with IRQ delivery.
        self.transport.lock_irqsave().ack_interrupt();
        self.complete()?;
        let now = clock();
        if self.inflating.as_ref().is_some_and(|r| now >= r.deadline)
            || self.deflating.as_ref().is_some_and(|r| now >= r.deadline)
            || self.report.as_ref().is_some_and(|r| now >= r.deadline)
        {
            return Err(virtio_drivers::Error::IoError);
        }
        let target = self.target()?;
        let oom = OOM_REQUEST.load(Ordering::Acquire);
        if self.reclaimable() && oom != self.oom_seen {
            self.cooldown = next_n_us_timer_jiffies(OOM_COOLDOWN_US);
            // Keep this request pending until an existing deflate finishes.
            if self.deflating.is_none() {
                self.oom_seen = oom;
                self.submit_deflate(BATCH_PAGES)?;
            }
        }
        if self.deflating.is_none() && self.actual > target {
            self.submit_deflate(self.actual - target)?;
        }
        let pressure_recovered = LockedFrameAllocator::pressure_snapshot().total_deficit() == 0;
        if self.inflating.is_none()
            && self.deflating.is_none()
            && self.actual < target
            && clock() >= self.cooldown
            && pressure_recovered
        {
            self.submit_inflate(target - self.actual)?;
        }
        if self.reporting.is_some() && self.report.is_none() {
            self.submit_report()?;
        }
        Ok(self.inflating.is_some() || self.deflating.is_some() || self.report.is_some())
    }

    /// Reset is the ownership barrier. On failure retain everything in this
    /// permanent device registry, including buffers and physical page owners.
    fn stop(&mut self) {
        if self.failed {
            return;
        }
        self.failed = true;
        if self.transport.lock_irqsave().reset().is_err() {
            // These pages remain owned/accounted, but a terminal device can
            // no longer service OOM requests. Remove only eligibility once.
            if self.reclaimable() {
                RECLAIMABLE.fetch_sub(self.actual, Ordering::Release);
            }
            log::error!("virtio-balloon reset did not complete; retaining DMA owners");
            return;
        }
        self.release_after_reset();
    }

    fn release_after_reset(&mut self) {
        // Remove effective-total accounting before any physical frame is
        // returned. Readers may span the operations, never invert them.
        let count = self.actual;
        if count != 0 {
            self.change_actual(count, false);
        }
        // Queue Drop releases coherent rings. Shares used here are direct
        // mapped, so reset does not require bounce-copy cleanup.
        self.inflate.take();
        self.deflate.take();
        self.reporting.take();
        self.inflating.take();
        self.deflating.take();
        self.pages.take();
        if let Some(request) = self.report.take() {
            page_reporting::finish(request.block, false);
        }
        if count != 0 {
            FREED.fetch_add(count as u64, Ordering::Release);
        }
    }
}

fn report_buffer(block: &mut ReportingBlock) -> &mut [u8] {
    // The reporting API exclusively isolates this entire block from Buddy.
    // A buffer is built only at submission/pop and never read by the CPU.
    let address =
        unsafe { MMArch::phys_2_virt(block.address()) }.expect("reporting block direct map");
    unsafe { slice::from_raw_parts_mut(address.data() as *mut u8, block.bytes()) }
}

fn worker_loop() -> i32 {
    let (waiter, waker) = Waiter::try_new_pair().expect("balloon waiter bootstrap failed");
    *WAKER.lock_irqsave() = Some(waker);
    loop {
        WORK_PENDING.store(false, Ordering::Release);
        let mut busy = false;
        let mut cursor = DEVICES.lock_irqsave().clone();
        while let Some(balloon) = cursor {
            cursor = balloon.next.clone();
            // Mutex::lock allocates a wait-queue registration even on its
            // fast path. The sole worker uses try_lock; debug readers also
            // never sleep while holding this device's progress executor.
            let Ok(mut device) = balloon.device.try_lock() else {
                busy = true;
                continue;
            };
            match device.step() {
                Ok(pending) => busy |= pending,
                Err(error) => {
                    log::error!("virtio-balloon stopped after queue/transport error: {error:?}");
                    device.stop();
                }
            }
        }
        if OOM_WAITING.load(Ordering::Acquire) {
            crate::libs::resource_wait::wake_all();
        }
        DEADLINE.store(
            next_n_us_timer_jiffies(if busy || OOM_WAITING.load(Ordering::Acquire) {
                POLL_US
            } else {
                IDLE_US
            })
            .max(1),
            Ordering::Release,
        );
        if !WORK_PENDING.load(Ordering::Acquire) {
            waiter.wait(false).expect("balloon worker wait failed");
        }
    }
}

pub(super) fn virtio_balloon(transport: VirtIOTransport, dev_id: Arc<DeviceId>) {
    if MMArch::PAGE_SIZE != 4096 {
        log::warn!("virtio-balloon requires 4KiB kernel pages");
        return;
    }
    if let Err(error) = initialize(transport, dev_id) {
        log::warn!("virtio-balloon initialization failed: {error:?}");
    }
}

fn initialize(mut transport: VirtIOTransport, dev_id: Arc<DeviceId>) -> Result<(), SystemError> {
    // No ring is published until reset is confirmed.
    transport
        .reset()
        .map_err(super::virtio_drivers_error_to_system_error)?;
    let features = transport.begin_init(Features::all());
    // Read raw offered bits after ACKNOWLEDGE/DRIVER. Declining stats/hint
    // does not remove their existing slots from the host's queue topology.
    let offered = transport.read_device_features();
    let reporting_index =
        2 + u16::from(offered & (1 << 1) != 0) + u16::from(offered & (1 << 3) != 0);
    if !transport.get_status().contains(DeviceStatus::FEATURES_OK) {
        let _ = transport.reset();
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    transport
        .config_space::<[u32; 2]>()
        .map_err(super::virtio_drivers_error_to_system_error)?;
    // IRQ reservation/installation may allocate; use the still-private
    // transport without an IRQ-save register lock. DRIVER_OK is not set.
    let setup = match transport.setup_irq(dev_id.clone()) {
        Ok(setup) => Some(setup),
        Err(error) => {
            // MSI/MSI-X is optional for PCI balloon devices. A safe fallback
            // must explicitly mask INTx, including configuration interrupts.
            if let Err(poll_error) = transport.prepare_polling() {
                if transport.reset().is_err() {
                    mem::forget(transport);
                }
                return Err(if poll_error == SystemError::EBUSY {
                    poll_error
                } else {
                    error
                });
            }
            None
        }
    };
    // MSI-X routing is assigned by queue_set, so IRQ setup must precede
    // queue construction even though DRIVER_OK/callback publication is later.
    // Queue construction may allocate/compact coherent memory; do it before
    // exposing transport to the IRQ callback and without any IRQ-save lock.
    let mut inflate = None;
    let mut deflate = None;
    let mut reporting = None;
    let result = (|| {
        inflate = Some(Queue::new(&mut transport, 0, false, false)?);
        deflate = Some(Queue::new(&mut transport, 1, false, false)?);
        if features.contains(Features::REPORTING) {
            reporting = Some(Queue::new(&mut transport, reporting_index, false, false)?);
        }
        Ok::<(), virtio_drivers::Error>(())
    })();
    if let Err(error) = result {
        if transport.reset().is_err() {
            mem::forget((inflate, deflate, reporting, transport, setup));
        }
        return Err(super::virtio_drivers_error_to_system_error(error));
    }
    if setup.is_none() {
        for queue in [&mut inflate, &mut deflate, &mut reporting]
            .into_iter()
            .flatten()
        {
            queue.set_dev_notify(false);
        }
    }
    let polling = setup.is_none();
    let (irq_lease, deferred_irq) = setup
        .map(|setup| setup.into_parts())
        .unwrap_or((None, None));
    let dedicated_irq = irq_lease.is_some();
    let transport = Arc::new(SpinLock::new(transport));
    let mut device = Device {
        transport: transport.clone(),
        features,
        inflate,
        deflate,
        reporting,
        reporting_index,
        inflating: None,
        deflating: None,
        report: None,
        pages: None,
        actual: 0,
        oom_seen: OOM_REQUEST.load(Ordering::Acquire),
        cooldown: 0,
        failed: false,
        reported: 0,
        _irq_lease: irq_lease,
    };
    let callback = Arc::new(Interrupt {
        transport: transport.clone(),
        dedicated: dedicated_irq,
    });
    if let Err(error) = virtio_irq_manager().register_callback(dev_id.clone(), callback) {
        if transport.lock_irqsave().reset().is_err() {
            mem::forget(device);
        }
        return Err(error);
    }
    let irq_result = if let Some(deferred) = deferred_irq {
        deferred.install(dev_id.clone())
    } else {
        Ok(())
    };
    if let Err(error) = irq_result {
        if transport.lock_irqsave().reset().is_err() {
            // The callback/IRQ lease must also remain live on unconfirmed reset.
            mem::forget(device);
        } else {
            virtio_irq_manager().unregister_callback(&dev_id);
        }
        return Err(error);
    }
    // Probe is serial. Bootstrap before publishing this device so failed
    // worker creation can roll back the complete unpublished resource owner.
    let worker_result = if WORKER.lock_irqsave().is_none() {
        let closure = KernelThreadClosure::StaticEmptyClosure((&(worker_loop as fn() -> i32), ()));
        match KernelThreadMechanism::create_on_cpu(
            closure,
            "virtio_balloon".to_string(),
            crate::smp::core::smp_get_processor_id(),
        ) {
            Some(worker) => {
                *WORKER.lock_irqsave() = Some(worker.clone());
                ProcessManager::wakeup(&worker)
            }
            None => Err(SystemError::ENOMEM),
        }
    } else {
        Ok(())
    };
    if let Err(error) = worker_result {
        if transport.lock_irqsave().reset().is_err() {
            // The callback/IRQ lease must also remain live on unconfirmed reset.
            mem::forget(device);
        } else {
            virtio_irq_manager().unregister_callback(&dev_id);
        }
        return Err(error);
    }
    let next = DEVICES.lock_irqsave().clone();
    let balloon = Arc::new(Balloon {
        device: Mutex::new(device),
        next,
    });
    // Until registry publication the worker cannot access this device. An
    // early interrupt can only request a harmless extra worker iteration.
    transport.lock_irqsave().finish_init();
    *DEVICES.lock_irqsave() = Some(balloon);
    request_work();
    log::info!(
        "virtio-balloon initialized (reporting={}, deflate_on_oom={}, polling={})",
        features.contains(Features::REPORTING),
        features.contains(Features::DEFLATE_ON_OOM),
        polling
    );
    Ok(())
}

#[cfg(feature = "mm_selftest")]
pub(crate) fn selftest_report() -> alloc::string::String {
    use core::fmt::Write;
    let mut output = alloc::string::String::new();
    let mut index = 0;
    let mut cursor = DEVICES.lock_irqsave().clone();
    while let Some(balloon) = cursor {
        cursor = balloon.next.clone();
        let Ok(device) = balloon.device.try_lock() else {
            let _ = writeln!(output, "device={} busy=1", index);
            index += 1;
            continue;
        };
        let snapshot = (
            device.actual,
            device.features.contains(Features::REPORTING),
            device.reported,
            device.reclaimable(),
            device.failed,
        );
        drop(device);
        // Formatting may allocate/reclaim. Never retain a device lock while
        // doing so, even for a diagnostics-only read.
        let _ = writeln!(output, "device={} registered=1 actual_pages={} reporting_negotiated={} reporting_completed={} deflate_on_oom_negotiated={} failed={}", index, snapshot.0, snapshot.1, snapshot.2, snapshot.3, snapshot.4);
        index += 1;
    }
    if index == 0 {
        output.push_str("no virtio balloon devices\n");
    }
    output
}
