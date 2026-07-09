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
            kthread::{enqueue_tty_rx_to_target_from_irq, tty_input_target_room, TtyInputTarget},
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
    exception::{
        irqdesc::IrqReturn,
        tasklet::{tasklet_schedule, Tasklet, TaskletData},
        IrqNumber,
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_POSTCORE,
    libs::{
        lazy_init::Lazy,
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::page::PAGE_4K_SIZE,
};
use alloc::boxed::Box;
use alloc::string::String;
use alloc::string::ToString;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use bitmap::{static_bitmap, traits::BitMapOps};
use core::fmt::Debug;
use core::fmt::Formatter;
use core::{
    any::Any,
    ptr::NonNull,
    sync::atomic::{compiler_fence, Ordering},
};
use system_error::SystemError;
use unified_init::macros::unified_init;
use virtio_drivers::{
    queue::VirtQueue,
    transport::{DeviceStatus, Transport},
    Error as VirtioError,
};

const VIRTIO_CONSOLE_BASENAME: &str = "virtio_console";
const HVC_MINOR: u32 = 0;
const VIRTIO_CONSOLE_RX_IRQ_LIMIT: usize = PAGE_4K_SIZE;
const VIRTIO_CONSOLE_RECEIVEQ_PORT_0: u16 = 0;
const VIRTIO_CONSOLE_TRANSMITQ_PORT_0: u16 = 1;
const VIRTIO_CONSOLE_QUEUE_SIZE: usize = 2;
const VIRTIO_CONSOLE_F_RING_EVENT_IDX: u64 = 1 << 29;
const VIRTIO_CONSOLE_OUTBUF_SIZE: usize = PAGE_4K_SIZE;
const VIRTIO_CONSOLE_TX_CHUNK: usize = 256;
const VIRTIO_CONSOLE_TX_FLUSH_BUDGET: usize = PAGE_4K_SIZE;
const VIRTIO_CONSOLE_IRQ_TX_FLUSH_BUDGET: usize = VIRTIO_CONSOLE_TX_CHUNK;

static mut VIRTIO_CONSOLE_DRIVER: Option<Arc<VirtIOConsoleDriver>> = None;
static mut TTY_HVC_DRIVER: Option<Arc<TtyDriver>> = None;

lazy_static! {
    static ref VIRTIO_CONSOLE_RX_RETRY_TASKLET: Arc<Tasklet> =
        Tasklet::new(virtio_console_rx_retry_tasklet, 0, None);
}

pub fn retry_virtio_console_input() {
    tasklet_schedule(&VIRTIO_CONSOLE_RX_RETRY_TASKLET);
}

fn virtio_console_rx_retry_tasklet(_data: usize, _data_obj: Option<Arc<dyn TaskletData>>) {
    let Some(driver) = (unsafe { VIRTIO_CONSOLE_DRIVER.as_ref().cloned() }) else {
        return;
    };
    driver.retry_input();
}

#[inline(always)]
fn tty_hvc_driver() -> &'static Arc<TtyDriver> {
    unsafe { TTY_HVC_DRIVER.as_ref().unwrap() }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VirtIOConsoleInfo {
    rows: u16,
    columns: u16,
}

#[repr(C)]
struct VirtIOConsoleConfig {
    cols: u16,
    rows: u16,
    max_nr_ports: u32,
    emerg_wr: u32,
}

struct DragonVirtIOConsole {
    transport: VirtIOTransport,
    config_space: NonNull<VirtIOConsoleConfig>,
    receiveq: VirtQueue<HalImpl, VIRTIO_CONSOLE_QUEUE_SIZE>,
    transmitq: VirtQueue<HalImpl, VIRTIO_CONSOLE_QUEUE_SIZE>,
    queue_buf_rx: Box<[u8; PAGE_4K_SIZE]>,
    tx_buf: Box<[u8; VIRTIO_CONSOLE_TX_CHUNK]>,
    tx_len: usize,
    tx_token: Option<u16>,
    cursor: usize,
    pending_len: usize,
    receive_token: Option<u16>,
}

impl DragonVirtIOConsole {
    fn new(mut transport: VirtIOTransport) -> Result<Self, VirtioError> {
        transport.set_status(DeviceStatus::empty());
        transport.set_status(DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER);

        let device_features = transport.read_device_features();
        let negotiated_features = device_features & VIRTIO_CONSOLE_F_RING_EVENT_IDX;
        transport.write_driver_features(negotiated_features);
        transport.set_status(
            DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER | DeviceStatus::FEATURES_OK,
        );
        transport.set_guest_page_size(PAGE_4K_SIZE as u32);

        let event_idx = negotiated_features & VIRTIO_CONSOLE_F_RING_EVENT_IDX != 0;
        let config_space = transport.config_space::<VirtIOConsoleConfig>()?;
        let receiveq = VirtQueue::new(
            &mut transport,
            VIRTIO_CONSOLE_RECEIVEQ_PORT_0,
            false,
            event_idx,
        )?;
        let transmitq = VirtQueue::new(
            &mut transport,
            VIRTIO_CONSOLE_TRANSMITQ_PORT_0,
            false,
            event_idx,
        )?;

        let queue_buf_rx = Box::new([0; PAGE_4K_SIZE]);
        let tx_buf = Box::new([0; VIRTIO_CONSOLE_TX_CHUNK]);
        transport.finish_init();

        let mut console = Self {
            transport,
            config_space,
            receiveq,
            transmitq,
            queue_buf_rx,
            tx_buf,
            tx_len: 0,
            tx_token: None,
            cursor: 0,
            pending_len: 0,
            receive_token: None,
        };
        console.poll_retrieve()?;
        Ok(console)
    }

    fn info(&self) -> VirtIOConsoleInfo {
        let config = self.config_space.as_ptr();
        // SAFETY: config_space is provided by the virtio transport for this console device.
        unsafe {
            VirtIOConsoleInfo {
                columns: core::ptr::read_volatile(core::ptr::addr_of!((*config).cols)),
                rows: core::ptr::read_volatile(core::ptr::addr_of!((*config).rows)),
            }
        }
    }

    fn poll_retrieve(&mut self) -> Result<(), VirtioError> {
        if self.receive_token.is_none() && self.cursor == self.pending_len {
            // SAFETY: queue_buf_rx remains alive until the matching pop_used completes.
            self.receive_token = Some(unsafe {
                self.receiveq
                    .add(&[], &mut [self.queue_buf_rx.as_mut_slice()])
            }?);
            if self.receiveq.should_notify() {
                self.transport.notify(VIRTIO_CONSOLE_RECEIVEQ_PORT_0);
            }
        }
        Ok(())
    }

    fn finish_receive(&mut self) -> Result<bool, VirtioError> {
        let mut has_new_data = false;
        if let Some(receive_token) = self.receive_token {
            if self.receiveq.peek_used() == Some(receive_token) {
                // SAFETY: this pops the same RX buffer registered in poll_retrieve().
                let len = unsafe {
                    self.receiveq.pop_used(
                        receive_token,
                        &[],
                        &mut [self.queue_buf_rx.as_mut_slice()],
                    )?
                };
                has_new_data = true;
                self.cursor = 0;
                self.pending_len = len as usize;
                self.receive_token.take();
            }
        }
        Ok(has_new_data)
    }

    fn recv(&mut self, pop: bool) -> Result<Option<u8>, VirtioError> {
        self.finish_receive()?;
        if self.cursor == self.pending_len {
            return Ok(None);
        }
        let ch = self.queue_buf_rx[self.cursor];
        if pop {
            self.cursor += 1;
            self.poll_retrieve()?;
        }
        Ok(Some(ch))
    }

    fn pending_tx_len(&self) -> usize {
        if self.tx_token.is_some() {
            self.tx_len
        } else {
            0
        }
    }

    fn complete_tx(&mut self) -> Result<Option<usize>, VirtioError> {
        let Some(token) = self.tx_token else {
            return Ok(None);
        };
        match self.transmitq.peek_used() {
            None => return Ok(None),
            Some(used) if used == token => {}
            Some(_) => return Err(VirtioError::WrongToken),
        }

        let tx_len = self.tx_len;
        let tx_buf = &self.tx_buf[..tx_len];
        // SAFETY: tx_buf is the same stable DMA buffer submitted by submit_tx().
        unsafe {
            self.transmitq.pop_used(token, &[tx_buf], &mut [])?;
        }
        self.tx_token = None;
        self.tx_len = 0;
        Ok(Some(tx_len))
    }

    fn submit_tx(&mut self, buf: &[u8]) -> Result<(), VirtioError> {
        if buf.is_empty() || self.tx_token.is_some() {
            return Ok(());
        }
        let len = buf.len().min(self.tx_buf.len());
        self.tx_buf[..len].copy_from_slice(&buf[..len]);
        let tx_buf = &self.tx_buf[..len];
        // SAFETY: tx_buf belongs to this device and is not modified again until complete_tx()
        // observes and pops the matching used descriptor.
        let token = unsafe { self.transmitq.add(&[tx_buf], &mut [])? };
        self.tx_token = Some(token);
        self.tx_len = len;
        if self.transmitq.should_notify() {
            self.transport.notify(VIRTIO_CONSOLE_TRANSMITQ_PORT_0);
        }
        Ok(())
    }

    fn enable_interrupts(&mut self) {
        self.receiveq.set_dev_notify(true);
        self.transmitq.set_dev_notify(true);
    }
}

impl Drop for DragonVirtIOConsole {
    fn drop(&mut self) {
        self.transport.queue_unset(VIRTIO_CONSOLE_RECEIVEQ_PORT_0);
        self.transport.queue_unset(VIRTIO_CONSOLE_TRANSMITQ_PORT_0);
    }
}

pub fn virtio_console(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    log::debug!(
        "virtio_console: dev_id: {:?}, parent: {:?}",
        dev_id,
        dev_parent.as_ref().map(|x| x.name())
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

impl Debug for VirtIOConsoleDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtIOConsoleDevice")
            .field(
                "devname",
                &self
                    .dev_name
                    .try_get()
                    .map(|x| x.as_str())
                    .unwrap_or("uninitialized"),
            )
            .field("dev_id", &self.dev_id.id())
            .finish()
    }
}

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
        let device_inner = DragonVirtIOConsole::new(transport);
        if let Err(e) = device_inner {
            log::error!("VirtIOConsoleDevice '{dev_id:?}' create failed: {:?}", e);
            return None;
        }

        let mut device_inner = device_inner.unwrap();
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
                input_target: None,
                input_rx_paused: false,
                output_tty: Weak::new(),
                outbuf: Vec::with_capacity(VIRTIO_CONSOLE_OUTBUF_SIZE),
                outbuf_size: VIRTIO_CONSOLE_OUTBUF_SIZE,
                flush_pending: false,
            }),
        });

        Some(dev)
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOConsoleDevice> {
        self.inner.lock_irqsave()
    }

    fn flush_output_budget(
        &self,
        budget: usize,
    ) -> Result<(usize, Option<Arc<TtyCore>>), SystemError> {
        let mut inner = self.inner();
        let was_full = inner.write_room() == 0;
        let flushed = match inner.flush_output_locked(budget) {
            Ok(flushed) => flushed,
            Err(err) => {
                let tty = inner.output_tty.upgrade();
                drop(inner);
                Self::wake_output_tty(tty);
                return Err(err);
            }
        };
        let should_wakeup = flushed > 0 && (was_full || inner.write_room() > 0);
        let tty = if should_wakeup {
            inner.output_tty.upgrade()
        } else {
            None
        };
        Ok((flushed, tty))
    }

    fn wake_output_tty(tty: Option<Arc<TtyCore>>) {
        if let Some(tty) = tty {
            tty.tty_wakeup();
        }
    }

    fn pump_input(&self, limit: usize) -> Result<usize, SystemError> {
        let mut received = 0;
        while received < limit {
            let mut inner = self.inner();
            let Some(target) = inner.input_target.clone() else {
                inner.input_rx_paused = false;
                break;
            };

            if tty_input_target_room(&target) == 0 {
                inner.input_rx_paused = true;
                break;
            }

            let c = match inner.device_inner.recv(false) {
                Ok(Some(c)) => c,
                Ok(None) => {
                    inner.input_rx_paused = false;
                    break;
                }
                Err(err) => {
                    inner.input_rx_paused = false;
                    return Err(virtio_drivers_error_to_system_error(err));
                }
            };

            if enqueue_tty_rx_to_target_from_irq(&target, &[c]) != 1 {
                inner.input_rx_paused = true;
                break;
            }

            match inner.device_inner.recv(true) {
                Ok(Some(_)) => {
                    inner.input_rx_paused = false;
                    received += 1;
                }
                Ok(None) => {
                    inner.input_rx_paused = false;
                    break;
                }
                Err(err) => {
                    inner.input_rx_paused = false;
                    return Err(virtio_drivers_error_to_system_error(err));
                }
            }
        }
        if limit != 0 && received == limit {
            self.inner().input_rx_paused = true;
            retry_virtio_console_input();
        }
        Ok(received)
    }
}

struct InnerVirtIOConsoleDevice {
    device_inner: DragonVirtIOConsole,
    virtio_index: Option<VirtIODeviceIndex>,
    name: Option<String>,
    device_common: DeviceCommonData,
    kobject_common: KObjectCommonData,
    irq: Option<IrqNumber>,
    input_target: Option<TtyInputTarget>,
    input_rx_paused: bool,
    output_tty: Weak<TtyCore>,
    outbuf: Vec<u8>,
    outbuf_size: usize,
    flush_pending: bool,
}

impl Debug for InnerVirtIOConsoleDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerVirtIOConsoleDevice")
            .field("virtio_index", &self.virtio_index)
            .field("name", &self.name)
            .field("device_common", &self.device_common)
            .field("kobject_common", &self.kobject_common)
            .field("irq", &self.irq)
            .field("input_target", &self.input_target)
            .field("input_rx_paused", &self.input_rx_paused)
            .field("outbuf_len", &self.outbuf.len())
            .field("outbuf_size", &self.outbuf_size)
            .field("flush_pending", &self.flush_pending)
            .finish()
    }
}

impl InnerVirtIOConsoleDevice {
    fn write_room(&self) -> usize {
        self.outbuf_size.saturating_sub(self.outbuf.len())
    }

    fn chars_in_buffer(&self) -> usize {
        self.outbuf.len()
    }

    fn flush_output_locked(&mut self, max_bytes: usize) -> Result<usize, SystemError> {
        if self.outbuf.is_empty() && self.device_inner.pending_tx_len() == 0 {
            self.flush_pending = false;
            return Ok(0);
        }

        let mut total = 0;
        while total < max_bytes {
            let Some(done) = self
                .device_inner
                .complete_tx()
                .map_err(virtio_drivers_error_to_system_error)?
            else {
                break;
            };
            let drain_len = done.min(self.outbuf.len());
            if drain_len != 0 {
                self.outbuf.drain(0..drain_len);
                total += drain_len;
            }
        }

        if self.device_inner.pending_tx_len() == 0 && !self.outbuf.is_empty() && max_bytes != 0 {
            let send_len = self
                .outbuf
                .len()
                .min(VIRTIO_CONSOLE_TX_CHUNK)
                .min(max_bytes.saturating_sub(total).max(1));
            match self.device_inner.submit_tx(&self.outbuf[..send_len]) {
                Ok(()) => {}
                Err(VirtioError::QueueFull) | Err(VirtioError::NotReady) => {
                    self.flush_pending = true;
                }
                Err(err) => {
                    self.outbuf.clear();
                    self.flush_pending = false;
                    if total > 0 {
                        return Ok(total);
                    }
                    return Err(virtio_drivers_error_to_system_error(err));
                }
            }
        }

        self.flush_pending = !self.outbuf.is_empty() || self.device_inner.pending_tx_len() != 0;
        Ok(total)
    }

    fn discard_unsubmitted_output_locked(&mut self) {
        let keep_inflight = self.device_inner.pending_tx_len().min(self.outbuf.len());
        if self.outbuf.len() > keep_inflight {
            self.outbuf.truncate(keep_inflight);
        }
        self.flush_pending = keep_inflight != 0;
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
        let _ = self.pump_input(VIRTIO_CONSOLE_RX_IRQ_LIMIT);
        if let Ok((_, tty)) = self.flush_output_budget(VIRTIO_CONSOLE_IRQ_TX_FLUSH_BUDGET) {
            Self::wake_output_tty(tty);
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

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOConsoleDriver> {
        self.inner.lock()
    }

    fn retry_input(&self) {
        let devices = self.devices.read();
        for dev in devices.iter().flatten() {
            let paused = dev.inner().input_rx_paused;
            if paused {
                let _ = dev.pump_input(VIRTIO_CONSOLE_RX_IRQ_LIMIT);
            }
        }
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
        tty.core()
            .set_vc_index(vc.index().ok_or(SystemError::ENODEV)?);

        Ok(())
    }
}

#[derive(Debug)]
struct InnerVirtIOConsoleDriver {
    id_bmp: static_bitmap!(VirtIOConsoleDriver::MAX_DEVICES),
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

    fn write_room(&self, tty: &TtyCoreData) -> usize {
        let index = tty.index();
        if index >= VirtIOConsoleDriver::MAX_DEVICES {
            return 0;
        }

        self.devices.read()[index]
            .as_ref()
            .map(|dev| dev.inner().write_room())
            .unwrap_or(0)
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
        let mut accepted = 0;
        let mut wake_tty = None;
        let mut inner = dev.inner();

        while accepted < nr {
            if inner.write_room() == 0 {
                match inner.flush_output_locked(VIRTIO_CONSOLE_TX_FLUSH_BUDGET) {
                    Ok(flushed) => {
                        if flushed > 0 && inner.write_room() > 0 {
                            wake_tty = inner.output_tty.upgrade();
                        }
                    }
                    Err(err) => {
                        wake_tty = inner.output_tty.upgrade();
                        drop(inner);
                        VirtIOConsoleDevice::wake_output_tty(wake_tty);
                        if accepted > 0 {
                            return Ok(accepted);
                        }
                        return Err(err);
                    }
                }

                if inner.write_room() == 0 {
                    break;
                }
            }

            let copy_len = (nr - accepted).min(inner.write_room());
            if copy_len == 0 {
                break;
            }
            inner
                .outbuf
                .extend_from_slice(&buf[accepted..accepted + copy_len]);
            accepted += copy_len;

            let was_full = inner.write_room() == 0;
            match inner.flush_output_locked(VIRTIO_CONSOLE_TX_FLUSH_BUDGET) {
                Ok(flushed) => {
                    if flushed > 0 && (was_full || inner.write_room() > 0) {
                        wake_tty = inner.output_tty.upgrade();
                    }
                    if flushed == 0 && inner.write_room() == 0 {
                        break;
                    }
                }
                Err(err) => {
                    wake_tty = inner.output_tty.upgrade();
                    drop(inner);
                    VirtIOConsoleDevice::wake_output_tty(wake_tty);
                    if accepted > 0 {
                        return Ok(accepted);
                    }
                    return Err(err);
                }
            }
        }

        drop(inner);
        VirtIOConsoleDevice::wake_output_tty(wake_tty);

        Ok(accepted)
    }

    fn flush_chars(&self, tty: &TtyCoreData) {
        let index = tty.index();
        if index >= VirtIOConsoleDriver::MAX_DEVICES {
            return;
        }

        if let Some(dev) = self.devices.read()[index].clone() {
            if let Ok((_, wake_tty)) = dev.flush_output_budget(VIRTIO_CONSOLE_TX_FLUSH_BUDGET) {
                VirtIOConsoleDevice::wake_output_tty(wake_tty);
            }
        }
    }

    fn chars_in_buffer(&self, tty: &TtyCoreData) -> usize {
        let index = tty.index();
        if index >= VirtIOConsoleDriver::MAX_DEVICES {
            return 0;
        }

        self.devices.read()[index]
            .as_ref()
            .map(|dev| dev.inner().chars_in_buffer())
            .unwrap_or(0)
    }

    fn ioctl(&self, _tty: Arc<TtyCore>, _cmd: u32, _arg: usize) -> Result<(), SystemError> {
        Err(SystemError::ENOIOCTLCMD)
    }

    fn close(&self, tty: Arc<TtyCore>) -> Result<(), SystemError> {
        let index = tty.core().index();
        if index >= VirtIOConsoleDriver::MAX_DEVICES {
            return Ok(());
        }

        let dev = self.devices.read()[index].clone();
        tty.ldisc().flush_buffer(tty.clone())?;

        if let Some(dev) = dev {
            match dev.flush_output_budget(VIRTIO_CONSOLE_TX_FLUSH_BUDGET) {
                Ok((_, wake_tty)) => {
                    let close_wake = {
                        let mut inner = dev.inner();
                        inner.discard_unsubmitted_output_locked();
                        inner.output_tty.upgrade()
                    };
                    VirtIOConsoleDevice::wake_output_tty(wake_tty.or(close_wake));
                }
                Err(_) => {
                    let mut inner = dev.inner();
                    inner.discard_unsubmitted_output_locked();
                    let wake_tty = inner.output_tty.upgrade();
                    drop(inner);
                    VirtIOConsoleDevice::wake_output_tty(wake_tty);
                }
            }
        }

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
        self.do_install(driver, tty.clone(), vc.clone())
            .inspect_err(|_| {
                vc_manager().free(vc_index);
                let mut inner = dev.inner();
                inner.input_target = None;
                inner.output_tty = Weak::new();
            })?;
        {
            let mut inner = dev.inner();
            inner.input_target = Some(TtyInputTarget::new(vc_index, vc.port()));
            inner.input_rx_paused = false;
            inner.output_tty = Arc::downgrade(&tty);
        }
        let _ = dev.pump_input(VIRTIO_CONSOLE_RX_IRQ_LIMIT);

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
            panic!(
                "VirtIOConsoleDriver::add_device() failed: dev_name has already initialized for device: '{:?}'",
                virtio_con_dev.dev_id(),
            );
        }
        let mut inner = self.inner();
        let dev_name = inner.alloc_id();
        if dev_name.is_none() {
            panic!(
                "Failed to allocate ID for VirtIO console device: '{:?}', virtio console device limit exceeded.",
                virtio_con_dev.dev_id()
            )
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
            panic!(
                "VirtIOConsoleDriver::add_device() failed: device slot already occupied at index: {}",
                index
            );
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
