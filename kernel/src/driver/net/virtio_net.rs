use core::{
    any::Any,
    fmt::{Debug, Formatter},
};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use log::{debug, error};
use napi_state::CompleteState;
use smoltcp::{iface, phy, wire};
use unified_init::macros::unified_init;
use virtio_drivers::{
    device::net::VirtIONetRaw,
    transport::{DeviceStatus, DeviceType as VirtioDeviceType, Transport},
    PhysAddr,
};

use super::{Iface, NetDeivceState, NetDeviceCommonData, Operstate};
use crate::{
    arch::rand::rand,
    driver::{
        base::{
            class::Class,
            device::{
                bus::Bus,
                driver::{Driver, DriverCommonData},
                Device, DeviceCommonData, DeviceId, DeviceType, IdTable,
            },
            kobject::{KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        net::{
            napi::{
                __napi_schedule, napi_complete_state, napi_disable, napi_schedule,
                napi_schedule_prep, NapiStruct,
            },
            register_netdevice,
            types::InterfaceFlags,
        },
        virtio::{
            irq::virtio_irq_manager,
            sysfs::{virtio_bus, virtio_device_manager, virtio_driver_manager},
            transport::{DeferredVirtioIrq, VirtIOTransport},
            virtio_impl::HalImpl,
            VirtIODevice, VirtIODeviceIndex, VirtIODriver, VirtIODriverCommonData, VirtioDeviceId,
            VIRTIO_VENDOR_ID,
        },
    },
    exception::{irqdesc::IrqReturn, IrqNumber},
    filesystem::{kernfs::KernFSInode, sysfs::AttributeGroup},
    init::initcall::INITCALL_POSTCORE,
    libs::{
        rwlock::RwLock,
        rwsem::{RwSemReadGuard, RwSemWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::dma::{DmaAllocOptions, DmaBuffer, DmaDirection},
    net::generate_iface_id,
    process::namespace::net_namespace::INIT_NET_NAMESPACE,
    time::Instant,
};
use system_error::SystemError;

static mut VIRTIO_NET_DRIVER: Option<Arc<VirtIONetDriver>> = None;

const VIRTIO_NET_BASENAME: &str = "virtio_net";
const VIRTIO_NET_QUEUE_SIZE: usize = 64;
const VIRTIO_NET_BUFFER_SIZE: usize = 4096;
const VIRTIO_NET_IP_MTU: usize = 1500;
// smoltcp expects the complete Ethernet frame size here. Without
// VIRTIO_NET_F_MTU, Linux exposes the standard 1500-byte IP MTU.
const VIRTIO_NET_MAX_FRAME_SIZE: usize = VIRTIO_NET_IP_MTU + 14;
const VIRTIO_NET_RX_QUEUE: u16 = 0;
const VIRTIO_NET_TX_QUEUE: u16 = 1;

#[inline(always)]
#[allow(dead_code)]
fn virtio_net_driver() -> Arc<VirtIONetDriver> {
    unsafe { VIRTIO_NET_DRIVER.as_ref().unwrap().clone() }
}

/// Network-only transport wrapper which resets the device before its DMA
/// buffers are released.
struct VirtioNetTransport(VirtIOTransport, bool);

impl VirtioNetTransport {
    fn reset_device(&mut self) {
        if self.1 {
            return;
        }
        self.0.set_status(DeviceStatus::empty());
        while self.0.get_status() != DeviceStatus::empty() {
            core::hint::spin_loop();
        }
        self.1 = true;
    }
}

impl Transport for VirtioNetTransport {
    fn device_type(&self) -> VirtioDeviceType {
        self.0.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.0.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.0.write_driver_features(driver_features)
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.0.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.0.notify(queue)
    }

    fn get_status(&self) -> DeviceStatus {
        self.0.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.0.set_status(status)
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.0.set_guest_page_size(guest_page_size)
    }

    fn requires_legacy_layout(&self) -> bool {
        self.0.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.0
            .queue_set(queue, size, descriptors, driver_area, device_area)
    }

    fn queue_unset(&mut self, queue: u16) {
        // VirtIONetRaw unsets queues from its Drop implementation. DragonOS's
        // PCI transport cannot legally rewrite active queue configuration, so
        // stop all device DMA before forwarding the first unset.
        self.reset_device();
        self.0.queue_unset(queue)
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.0.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> bool {
        self.0.ack_interrupt()
    }

    fn begin_init<F>(&mut self, supported_features: F) -> F
    where
        F: bitflags2::Flags<Bits = u64> + core::ops::BitAnd<Output = F> + Debug,
    {
        self.0.begin_init(supported_features)
    }

    fn finish_init(&mut self) {
        self.0.finish_init()
    }

    fn config_space<T: 'static>(&self) -> virtio_drivers::Result<core::ptr::NonNull<T>> {
        self.0.config_space()
    }
}

impl Drop for VirtioNetTransport {
    fn drop(&mut self) {
        self.reset_device();
    }
}

/// virtio net device
#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIONetDevice {
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIONetDevice>,
    locked_kobj_state: LockedKObjectState,
    device_inner: VirtIONicDeviceInner,

    // 指向对应的interface
    iface_ref: RwLock<Weak<VirtioInterface>>,

    /// 是否使用MSIX中断
    irq_is_msix: bool,
}

impl Debug for VirtIONetDevice {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtIONetDevice")
            .field("dev_id", &self.dev_id.id())
            .finish()
    }
}

unsafe impl Send for VirtIONetDevice {}
unsafe impl Sync for VirtIONetDevice {}

struct InnerVirtIONetDevice {
    name: Option<String>,
    virtio_index: Option<VirtIODeviceIndex>,
    kobj_common: KObjectCommonData,
    device_common: DeviceCommonData,
}

impl Debug for InnerVirtIONetDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerVirtIONetDevice").finish()
    }
}

impl VirtIONetDevice {
    pub(crate) fn new(
        transport: VirtIOTransport,
        dev_id: Arc<DeviceId>,
    ) -> Option<(Arc<Self>, Option<DeferredVirtioIrq>)> {
        // 设置中断
        let irq_setup = match transport.setup_irq(dev_id.clone()) {
            Ok(setup) => setup,
            Err(err) => {
                error!("VirtIONetDevice '{dev_id:?}' setup_irq failed: {:?}", err);
                return None;
            }
        };

        let irq_is_msix = transport.irq_is_msix();
        let driver_net = match VirtIoNetImpl::new(VirtioNetTransport(transport, false)) {
            Ok(net) => net,
            Err(err) => {
                error!("VirtIONet init failed: {err:?}");
                return None;
            }
        };
        let mac = wire::EthernetAddress::from_bytes(&driver_net.mac_address());
        debug!("VirtIONetDevice mac: {:?}", mac);
        let device_inner = VirtIONicDeviceInner::new(driver_net);
        device_inner.inner.lock_irqsave().inner.enable_interrupts();
        let dev = Arc::new(Self {
            dev_id,
            inner: SpinLock::new(InnerVirtIONetDevice {
                name: None,
                virtio_index: None,
                kobj_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            locked_kobj_state: LockedKObjectState::default(),
            device_inner,
            iface_ref: RwLock::new(Weak::new()),
            irq_is_msix,
        });

        // dev.set_driver(Some(Arc::downgrade(&virtio_net_driver()) as Weak<dyn Driver>));

        return Some((dev, irq_setup.into_deferred()));
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIONetDevice> {
        return self.inner.lock();
    }

    pub fn set_iface(&self, iface: &Arc<VirtioInterface>) {
        *self.iface_ref.write() = Arc::downgrade(iface);
    }

    pub fn iface(&self) -> Option<Arc<VirtioInterface>> {
        self.iface_ref.read().upgrade()
    }
}

impl KObject for VirtIONetDevice {
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
}

impl Device for VirtIONetDevice {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(VIRTIO_NET_BASENAME.to_string(), None)
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

    fn attribute_groups(&self) -> Option<&'static [&'static dyn AttributeGroup]> {
        None
    }
}

impl VirtIODevice for VirtIONetDevice {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError> {
        if !self.device_inner.ack_interrupt() && !self.irq_is_msix {
            log::debug!(
                "VirtIONetDevice '{:?}' ack_interrupt not set",
                self.dev_id.id()
            );
        }

        let Some(iface) = self.iface() else {
            error!(
                "VirtIONetDevice '{:?}' has no associated iface to handle irq",
                self.dev_id.id()
            );
            return Ok(IrqReturn::Handled);
        };

        let Some(napi) = iface.napi_struct() else {
            log::error!("Virtio net device {} has no napi_struct", iface.name());
            return Ok(IrqReturn::Handled);
        };

        // log::debug!("virtio net irq: schedule napi");
        napi_schedule(napi);

        // self.netns.wakeup_poll_thread();
        return Ok(IrqReturn::Handled);
    }

    fn dev_id(&self) -> &Arc<DeviceId> {
        return &self.dev_id;
    }

    fn set_device_name(&self, name: String) {
        self.inner().name = Some(name);
    }

    fn device_name(&self) -> String {
        self.inner()
            .name
            .clone()
            .unwrap_or_else(|| "virtio_net".to_string())
    }

    fn set_virtio_device_index(&self, index: VirtIODeviceIndex) {
        self.inner().virtio_index = Some(index);
    }

    fn virtio_device_index(&self) -> Option<VirtIODeviceIndex> {
        return self.inner().virtio_index;
    }

    fn device_type_id(&self) -> u32 {
        virtio_drivers::transport::DeviceType::Network as u32
    }

    fn vendor(&self) -> u32 {
        VIRTIO_VENDOR_ID.into()
    }

    fn irq(&self) -> Option<IrqNumber> {
        None
    }
}

type RawVirtioNet = VirtIONetRaw<HalImpl, VirtioNetTransport, VIRTIO_NET_QUEUE_SIZE>;

struct RxDmaBuffer {
    dma: DmaBuffer,
    packet_offset: usize,
    packet_len: usize,
}

impl RxDmaBuffer {
    fn packet(&self) -> &[u8] {
        &self.dma.as_slice()[self.packet_offset..self.packet_offset + self.packet_len]
    }
}

struct TxDmaBuffer {
    dma: DmaBuffer,
    used_len: usize,
}

pub struct VirtIoNetImpl {
    // Keep the raw driver first: its transport reset must run before DMA slots are dropped.
    inner: RawVirtioNet,
    rx_slots: Vec<Option<RxDmaBuffer>>,
    tx_inflight: Vec<Option<TxDmaBuffer>>,
    tx_free: Vec<TxDmaBuffer>,
    // Buffers submitted under a token that failed validation must stay alive
    // until reset; returning them to the CPU would create a DMA use-after-free.
    rx_quarantine: Vec<RxDmaBuffer>,
    tx_quarantine: Vec<TxDmaBuffer>,
    poisoned: bool,
}

impl VirtIoNetImpl {
    fn dma_buffer(direction: DmaDirection) -> Result<DmaBuffer, SystemError> {
        DmaBuffer::try_alloc_bytes(
            VIRTIO_NET_BUFFER_SIZE,
            DmaAllocOptions {
                direction,
                use_pool: false,
                ..Default::default()
            },
        )
    }

    fn new(mut transport: VirtioNetTransport) -> Result<Self, SystemError> {
        let rx_size = transport.max_queue_size(VIRTIO_NET_RX_QUEUE);
        let tx_size = transport.max_queue_size(VIRTIO_NET_TX_QUEUE);
        if rx_size < VIRTIO_NET_QUEUE_SIZE as u32 || tx_size < VIRTIO_NET_QUEUE_SIZE as u32 {
            error!(
                "virtio-net requires queue size {}, device offers rx={} tx={}",
                VIRTIO_NET_QUEUE_SIZE, rx_size, tx_size
            );
            return Err(SystemError::ENODEV);
        }

        // Allocate every fallible payload buffer before the device starts consuming descriptors.
        let mut rx_buffers = Vec::with_capacity(VIRTIO_NET_QUEUE_SIZE);
        let mut tx_free = Vec::with_capacity(VIRTIO_NET_QUEUE_SIZE);
        for _ in 0..VIRTIO_NET_QUEUE_SIZE {
            rx_buffers.push(RxDmaBuffer {
                dma: Self::dma_buffer(DmaDirection::FromDevice)?,
                packet_offset: 0,
                packet_len: 0,
            });
            tx_free.push(TxDmaBuffer {
                dma: Self::dma_buffer(DmaDirection::ToDevice)?,
                used_len: 0,
            });
        }

        let mut rx_slots = (0..VIRTIO_NET_QUEUE_SIZE).map(|_| None).collect::<Vec<_>>();
        let tx_inflight = (0..VIRTIO_NET_QUEUE_SIZE).map(|_| None).collect();
        let mut rx_quarantine = Vec::new();
        // Declared last so an initialization error drops/resets the raw device
        // before any buffer already submitted to it is released.
        let mut inner = RawVirtioNet::new(transport).map_err(|_| SystemError::EIO)?;

        for mut buffer in rx_buffers {
            // SAFETY: DmaBuffer is page-backed and stays owned by rx_slots until completion.
            let token = unsafe { inner.receive_begin(buffer.dma.as_mut_slice()) }
                .map_err(|_| SystemError::EIO)?;
            let Some(slot) = rx_slots.get_mut(token as usize) else {
                rx_quarantine.push(buffer);
                return Err(SystemError::EIO);
            };
            if slot.is_some() {
                rx_quarantine.push(buffer);
                return Err(SystemError::EIO);
            }
            *slot = Some(buffer);
        }

        Ok(Self {
            inner,
            rx_slots,
            tx_inflight,
            tx_free,
            rx_quarantine,
            tx_quarantine: Vec::new(),
            poisoned: false,
        })
    }

    fn mac_address(&self) -> [u8; 6] {
        self.inner.mac_address()
    }

    fn poison(&mut self, reason: &'static str) {
        if !self.poisoned {
            error!("virtio-net queue poisoned: {reason}");
            self.inner.disable_interrupts();
            self.poisoned = true;
        }
    }

    fn receive(&mut self) -> Result<Option<RxDmaBuffer>, SystemError> {
        if self.poisoned {
            return Err(SystemError::EIO);
        }
        let token = match self.inner.poll_receive_checked() {
            Ok(Some(token)) => token,
            Ok(None) => return Ok(None),
            Err(_) => {
                self.poison("RX completion token out of range");
                return Err(SystemError::EIO);
            }
        };
        let index = token as usize;
        let Some(slot) = self.rx_slots.get_mut(index) else {
            self.poison("RX token out of range");
            return Err(SystemError::EIO);
        };
        let Some(mut buffer) = slot.take() else {
            self.poison("RX token has no device-owned buffer");
            return Err(SystemError::EIO);
        };

        // SAFETY: This is the same allocation submitted for this token and it remained stable.
        let completed = unsafe {
            self.inner
                .receive_complete(token, buffer.dma.as_mut_slice())
        };
        let (header_len, packet_len) = match completed {
            Ok(lengths) => lengths,
            Err(_) => {
                self.rx_slots[index] = Some(buffer);
                self.poison("RX completion/token mismatch");
                return Err(SystemError::EIO);
            }
        };
        let Some(end) = header_len.checked_add(packet_len) else {
            self.poison("RX completion length overflow");
            return Err(SystemError::EIO);
        };
        if end > buffer.dma.len() {
            // The descriptor is reclaimed and the allocation is CPU-owned,
            // but an impossible used length is a device/queue integrity
            // failure. Quarantine the buffer and fail-stop instead of
            // silently returning it to an untrusted queue.
            self.rx_quarantine.push(buffer);
            self.poison("RX completion exceeds submitted DMA buffer");
            return Err(SystemError::EIO);
        }
        buffer.packet_offset = header_len;
        buffer.packet_len = packet_len;
        Ok(Some(buffer))
    }

    fn recycle_rx(&mut self, mut buffer: RxDmaBuffer) -> Result<(), SystemError> {
        if self.poisoned {
            return Err(SystemError::EIO);
        }
        buffer.packet_offset = 0;
        buffer.packet_len = 0;
        // SAFETY: ownership returns to the queue and the allocation stays in rx_slots.
        let token = unsafe { self.inner.receive_begin(buffer.dma.as_mut_slice()) }
            .map_err(|_| SystemError::EIO)?;
        let Some(slot) = self.rx_slots.get_mut(token as usize) else {
            self.rx_quarantine.push(buffer);
            self.poison("recycled RX token out of range");
            return Err(SystemError::EIO);
        };
        if slot.is_some() {
            self.rx_quarantine.push(buffer);
            self.poison("recycled RX token already occupied");
            return Err(SystemError::EIO);
        }
        *slot = Some(buffer);
        Ok(())
    }

    fn reap_tx_completions(&mut self) -> Result<usize, SystemError> {
        let mut completed = 0;
        loop {
            let token = match self.inner.poll_transmit_checked() {
                Ok(Some(token)) => token,
                Ok(None) => break,
                Err(_) => {
                    self.poison("TX completion token out of range");
                    return Err(SystemError::EIO);
                }
            };
            let index = token as usize;
            let Some(slot) = self.tx_inflight.get_mut(index) else {
                self.poison("TX completion token out of range");
                return Err(SystemError::EIO);
            };
            let Some(buffer) = slot.take() else {
                self.poison("TX completion token has no buffer");
                return Err(SystemError::EIO);
            };
            let result = unsafe {
                self.inner
                    .transmit_complete(token, &buffer.dma.as_slice()[..buffer.used_len])
            };
            if result.is_err() {
                self.tx_inflight[index] = Some(buffer);
                self.poison("TX completion/token mismatch");
                return Err(SystemError::EIO);
            }
            self.tx_free.push(buffer);
            completed += 1;
        }
        Ok(completed)
    }

    fn has_rx_completion(&mut self) -> Result<bool, SystemError> {
        match self.inner.poll_receive_checked() {
            Ok(token) => Ok(token.is_some()),
            Err(_) => {
                self.poison("RX completion token out of range");
                Err(SystemError::EIO)
            }
        }
    }

    fn reserve_tx(&mut self) -> Option<TxDmaBuffer> {
        if self.poisoned || self.reap_tx_completions().is_err() || self.tx_free.len() < 2 {
            return None;
        }
        self.tx_free.pop()
    }

    fn release_tx(&mut self, mut buffer: TxDmaBuffer) {
        buffer.used_len = 0;
        self.tx_free.push(buffer);
    }

    fn submit_tx(&mut self, buffer: TxDmaBuffer) -> Result<(), SystemError> {
        if self.poisoned {
            self.release_tx(buffer);
            return Err(SystemError::EIO);
        }
        let used_len = buffer.used_len;
        let submitted = unsafe {
            self.inner
                .transmit_begin(&buffer.dma.as_slice()[..used_len])
        };
        let token = match submitted {
            Ok(token) => token,
            Err(_) => {
                self.poison("reserved TX descriptor could not be submitted");
                self.release_tx(buffer);
                return Err(SystemError::EIO);
            }
        };
        let Some(slot) = self.tx_inflight.get_mut(token as usize) else {
            self.tx_quarantine.push(buffer);
            self.poison("submitted TX token out of range");
            return Err(SystemError::EIO);
        };
        if slot.is_some() {
            self.tx_quarantine.push(buffer);
            self.poison("submitted TX token already occupied");
            return Err(SystemError::EIO);
        }
        *slot = Some(buffer);
        Ok(())
    }
}

unsafe impl Send for VirtIoNetImpl {}
unsafe impl Sync for VirtIoNetImpl {}

/// Virtio网络设备驱动(加锁)
#[derive(Clone)]
pub struct VirtIONicDeviceInner {
    pub inner: Arc<SpinLock<VirtIoNetImpl>>,
    /// 指向所属网络接口的弱引用，用于 packet socket 分发
    iface: Arc<SpinLock<Weak<dyn super::Iface>>>,
}

impl Debug for VirtIONicDeviceInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtIONicDriver").finish()
    }
}

#[cast_to([sync] Iface)]
#[cast_to([sync] Device)]
#[derive(Debug)]
pub struct VirtioInterface {
    device_inner: VirtIONicDeviceInner,
    iface_common: super::IfaceCommon,
    inner: SpinLock<InnerVirtIOInterface>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug)]
struct InnerVirtIOInterface {
    kobj_common: KObjectCommonData,
    device_common: DeviceCommonData,
    netdevice_common: NetDeviceCommonData,
}

impl VirtioInterface {
    pub fn new(mut device_inner: VirtIONicDeviceInner) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mut iface_config = iface::Config::new(wire::HardwareAddress::Ethernet(
            wire::EthernetAddress(device_inner.inner.lock_irqsave().mac_address()),
        ));
        iface_config.random_seed = rand() as u64;

        let iface = iface::Interface::new(iface_config, &mut device_inner, Instant::now().into());

        let flags = InterfaceFlags::UP
            | InterfaceFlags::BROADCAST
            | InterfaceFlags::RUNNING
            | InterfaceFlags::MULTICAST
            | InterfaceFlags::LOWER_UP;
        let iface_name = format!("eth{}", iface_id);
        let iface = Arc::new(VirtioInterface {
            device_inner,
            locked_kobj_state: LockedKObjectState::default(),
            iface_common: super::IfaceCommon::new(
                iface_id,
                crate::driver::net::types::InterfaceType::ETHER,
                iface_name,
                VIRTIO_NET_IP_MTU,
                flags,
                iface,
            ),
            inner: SpinLock::new(InnerVirtIOInterface {
                kobj_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                netdevice_common: NetDeviceCommonData::default(),
            }),
        });

        // 设置 device_inner 对接口的弱引用，用于 packet socket 分发
        iface
            .device_inner
            .set_iface(Arc::downgrade(&iface) as Weak<dyn super::Iface>);

        // 设置napi struct
        let napi_struct = NapiStruct::new(iface.clone(), 64);
        *iface.common().napi_struct.write() = Some(napi_struct);

        iface
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIOInterface> {
        return self.inner.lock();
    }
}

impl Drop for VirtioInterface {
    fn drop(&mut self) {
        // 从全局的网卡接口信息表中删除这个网卡的接口信息
        // NET_DEVICES.write_irqsave().remove(&self.nic_id());
        if let Some(ns) = self.net_namespace() {
            ns.remove_device(&self.nic_id());
        }
    }
}

impl Device for VirtioInterface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }

    fn id_table(&self) -> IdTable {
        IdTable::new(VIRTIO_NET_BASENAME.to_string(), None)
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

impl VirtIONicDeviceInner {
    pub fn new(driver_net: VirtIoNetImpl) -> Self {
        let inner = Arc::new(SpinLock::new(driver_net));
        VirtIONicDeviceInner {
            inner,
            iface: Arc::new(SpinLock::new(Weak::<VirtioInterface>::new())),
        }
    }

    pub fn ack_interrupt(&self) -> bool {
        self.inner.lock_irqsave().inner.ack_interrupt()
    }

    /// 设置所属网络接口的引用
    pub fn set_iface(&self, iface: Weak<dyn super::Iface>) {
        *self.iface.lock() = iface;
    }

    /// 获取所属网络接口
    pub fn iface(&self) -> Option<Arc<dyn super::Iface>> {
        self.iface.lock().upgrade()
    }

    fn submit_frame<R, F>(
        &self,
        mut buffer: TxDmaBuffer,
        len: usize,
        fill: F,
    ) -> (R, Result<(), SystemError>)
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let tap_iface = self
            .iface()
            .filter(crate::net::socket::packet::packet_sockets_active);
        let mut driver = self.inner.lock_irqsave();
        let header_len = driver
            .inner
            .fill_buffer_header(buffer.dma.as_mut_slice())
            .expect("preallocated virtio TX buffer is smaller than its header");
        let frame_end = header_len
            .checked_add(len)
            .filter(|end| *end <= buffer.dma.len())
            .expect("smoltcp exceeded advertised virtio-net MTU");
        let result = fill(&mut buffer.dma.as_mut_slice()[header_len..frame_end]);
        let tap_frame =
            tap_iface.map(|iface| (iface, buffer.dma.as_slice()[header_len..frame_end].to_vec()));
        buffer.used_len = frame_end;
        let submit_result = driver.submit_tx(buffer);
        drop(driver);

        if submit_result.is_ok() {
            if let Some((iface, frame)) = tap_frame {
                crate::net::socket::packet::deliver_to_packet_sockets(
                    &iface,
                    &frame,
                    crate::net::socket::packet::PacketType::Outgoing,
                );
            }
        }
        (result, submit_result)
    }

    pub fn try_raw_transmit(&self, frame: &[u8]) -> Result<(), SystemError> {
        if frame.len() > VIRTIO_NET_MAX_FRAME_SIZE {
            return Err(SystemError::EMSGSIZE);
        }
        let buffer = self
            .inner
            .lock_irqsave()
            .reserve_tx()
            .ok_or(SystemError::ENOBUFS)?;
        let (_, result) = self.submit_frame(buffer, frame.len(), |buf| buf.copy_from_slice(frame));
        result
    }
}

pub struct VirtioNetRxToken {
    driver: VirtIONicDeviceInner,
    rx_buffer: Option<RxDmaBuffer>,
}

pub struct VirtioNetTxToken {
    driver: VirtIONicDeviceInner,
    tx_buffer: Option<TxDmaBuffer>,
}

impl VirtioNetRxToken {
    fn new(driver: VirtIONicDeviceInner, rx_buffer: RxDmaBuffer) -> Self {
        Self {
            driver,
            rx_buffer: Some(rx_buffer),
        }
    }
}

impl VirtioNetTxToken {
    fn deferred(driver: VirtIONicDeviceInner) -> Self {
        Self {
            driver,
            tx_buffer: None,
        }
    }

    fn reserved(driver: VirtIONicDeviceInner, tx_buffer: TxDmaBuffer) -> Self {
        Self {
            driver,
            tx_buffer: Some(tx_buffer),
        }
    }
}

impl phy::Device for VirtIONicDeviceInner {
    type RxToken<'a>
        = VirtioNetRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = VirtioNetTxToken
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut driver = self.inner.lock_irqsave();
        match driver.receive() {
            Ok(Some(rx_buffer)) => Some((
                VirtioNetRxToken::new(self.clone(), rx_buffer),
                VirtioNetTxToken::deferred(self.clone()),
            )),
            Ok(None) => None,
            Err(err) => {
                error!("VirtIO receive failed: {err:?}");
                None
            }
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        let buffer = self.inner.lock_irqsave().reserve_tx()?;
        Some(VirtioNetTxToken::reserved(self.clone(), buffer))
    }

    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut caps = phy::DeviceCapabilities::default();
        // 网卡的最大传输单元. 请与IP层的MTU进行区分。这个值应当是网卡的最大传输单元，而不是IP层的MTU。
        caps.max_transmission_unit = VIRTIO_NET_MAX_FRAME_SIZE;
        /*
           Maximum burst size, in terms of MTU.
           The network device is unable to send or receive bursts large than the value returned by this function.
           If None, there is no fixed limit on burst size, e.g. if network buffers are dynamically allocated.
        */
        caps.max_burst_size = None;
        return caps;
    }
}

impl phy::TxToken for VirtioNetTxToken {
    fn consume<R, F>(mut self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        if let Some(buffer) = self.tx_buffer.take() {
            let (result, _) = self.driver.submit_frame(buffer, len, f);
            return result;
        }

        // The token paired with an RX packet is intentionally lazy: RX must
        // continue to make progress even while the TX queue is saturated.
        // Allocate response storage only if smoltcp actually emits a reply,
        // then make a best-effort reservation after the RX packet is consumed.
        let mut frame = alloc::vec![0; len];
        let result = f(&mut frame);
        let _ = self.driver.try_raw_transmit(&frame);
        result
    }
}

impl phy::RxToken for VirtioNetRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        let rx_buf = self.rx_buffer.take().unwrap();
        let packet = rx_buf.packet();

        // 向注册的 packet socket 分发数据包
        if let Some(iface) = self.driver.iface() {
            let pkt_type = crate::net::socket::packet::classify_packet(packet, &iface);
            crate::net::socket::packet::deliver_to_packet_sockets(&iface, packet, pkt_type);
        }

        let result = f(packet);
        if let Err(err) = self.driver.inner.lock_irqsave().recycle_rx(rx_buf) {
            error!("virtio-net failed to recycle RX buffer: {err:?}");
        }
        result
    }
}

impl Drop for VirtioNetRxToken {
    fn drop(&mut self) {
        if let Some(buffer) = self.rx_buffer.take() {
            if let Err(err) = self.driver.inner.lock_irqsave().recycle_rx(buffer) {
                error!("virtio-net failed to recycle dropped RX token: {err:?}");
            }
        }
    }
}

impl Drop for VirtioNetTxToken {
    fn drop(&mut self) {
        if let Some(buffer) = self.tx_buffer.take() {
            self.driver.inner.lock_irqsave().release_tx(buffer);
        }
    }
}

/// @brief virtio-net 驱动的初始化与测试
pub fn virtio_net(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    let virtio_net_deivce = VirtIONetDevice::new(transport, dev_id);
    if let Some((virtio_net_deivce, deferred_irq)) = virtio_net_deivce {
        debug!("VirtIONetDevice '{:?}' created", virtio_net_deivce.dev_id);
        if let Some(dev_parent) = dev_parent {
            virtio_net_deivce.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
        }
        if let Some(deferred_irq) = deferred_irq {
            if let Err(err) = deferred_irq.install(virtio_net_deivce.dev_id().clone()) {
                error!(
                    "VirtIONetDevice '{:?}' setup_irq failed: {:?}",
                    virtio_net_deivce.dev_id(),
                    err
                );
                return;
            }
        }
        virtio_device_manager()
            .device_add(virtio_net_deivce.clone() as Arc<dyn VirtIODevice>)
            .expect("Add virtio net failed");
    }
}

impl Iface for VirtioInterface {
    fn common(&self) -> &super::IfaceCommon {
        &self.iface_common
    }

    fn mac(&self) -> wire::EthernetAddress {
        let mac: [u8; 6] = self.device_inner.inner.lock_irqsave().mac_address();
        return wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn iface_name(&self) -> String {
        return self.iface_common.name();
    }

    fn poll(&self) -> bool {
        // External virtio-net interfaces have one data-plane owner: NAPI.
        // Socket syscalls and the namespace timer poller only request work;
        // they must not enter smoltcp/device polling concurrently on another CPU.
        if let Some(napi) = self.napi_struct() {
            napi_schedule(napi);
        }
        false
    }

    fn poll_napi(&self, budget: usize) -> super::napi::NapiPollResult {
        let mut device = self.device_inner.clone();
        {
            let mut driver = device.inner.lock_irqsave();
            if let Err(err) = driver.reap_tx_completions() {
                error!("virtio-net TX completion failed before NAPI poll: {err:?}");
            }
            if driver.poisoned {
                // Return through the normal completion hook once so it can
                // atomically move this NAPI instance to DISABLE. Continuing
                // into smoltcp can otherwise report poll_at=Now forever while
                // the failed device refuses every transmit reservation.
                return super::napi::NapiPollResult::idle();
            }
        }
        self.iface_common.poll_napi(&mut device, budget)
    }

    fn napi_poll_begin(&self) {
        let mut driver = self.device_inner.inner.lock_irqsave();
        if !driver.poisoned {
            driver.inner.disable_interrupts();
        }
    }

    fn napi_complete(&self, napi: Arc<NapiStruct>) {
        let interrupt_state = {
            let mut driver = self.device_inner.inner.lock_irqsave();
            if driver.poisoned {
                drop(driver);
                napi_disable(&napi);
                return;
            }
            driver.inner.enable_interrupts_prepare()
        };

        match napi_complete_state(&napi) {
            CompleteState::Missed => {
                self.device_inner
                    .inner
                    .lock_irqsave()
                    .inner
                    .disable_interrupts();
                __napi_schedule(napi);
            }
            CompleteState::Completed => {
                let mut acquired = false;
                {
                    let mut driver = self.device_inner.inner.lock_irqsave();
                    let tx_completed = driver.reap_tx_completions().unwrap_or_else(|err| {
                        error!("virtio-net TX completion failed during NAPI complete: {err:?}");
                        0
                    });
                    let has_rx = driver.has_rx_completion().unwrap_or_else(|err| {
                        error!(
                            "virtio-net RX completion check failed during NAPI complete: {err:?}"
                        );
                        false
                    });
                    let has_work = driver.inner.interrupt_pending(interrupt_state)
                        || tx_completed != 0
                        || has_rx;
                    if has_work && napi_schedule_prep(&napi) {
                        driver.inner.disable_interrupts();
                        acquired = true;
                    }
                }
                if acquired {
                    __napi_schedule(napi);
                }
            }
        }
    }

    fn raw_transmit(&self, frame: &[u8]) -> Result<(), SystemError> {
        // submit_frame() owns the outgoing packet-socket tap for all virtio
        // transmit paths, including raw frames.
        self.device_inner.try_raw_transmit(frame)
    }

    // fn as_any_ref(&'static self) -> &'static dyn core::any::Any {
    //     return self;
    // }

    fn addr_assign_type(&self) -> u8 {
        return self.inner().netdevice_common.addr_assign_type;
    }

    fn net_device_type(&self) -> u16 {
        self.inner().netdevice_common.net_device_type = 1; // 以太网设备
        return self.inner().netdevice_common.net_device_type;
    }

    fn net_state(&self) -> NetDeivceState {
        return self.inner().netdevice_common.state;
    }

    fn set_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state |= state;
    }

    fn operstate(&self) -> Operstate {
        return self.inner().netdevice_common.operstate;
    }

    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }

    fn mtu(&self) -> usize {
        self.iface_common.mtu()
    }
}

impl KObject for VirtioInterface {
    fn as_any_ref(&self) -> &dyn core::any::Any {
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

    fn name(&self) -> String {
        self.iface_common.name()
    }

    fn set_name(&self, name: String) {
        self.iface_common.set_name(name);
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
        self.inner().kobj_common.kobj_type = ktype;
    }
}

#[unified_init(INITCALL_POSTCORE)]
fn virtio_net_driver_init() -> Result<(), SystemError> {
    let driver = VirtIONetDriver::new();
    virtio_driver_manager().register(driver.clone() as Arc<dyn VirtIODriver>)?;
    unsafe {
        VIRTIO_NET_DRIVER = Some(driver);
    }

    return Ok(());
}

#[derive(Debug)]
#[cast_to([sync] VirtIODriver)]
#[cast_to([sync] Driver)]
struct VirtIONetDriver {
    inner: SpinLock<InnerVirtIODriver>,
    kobj_state: LockedKObjectState,
}

impl VirtIONetDriver {
    pub fn new() -> Arc<Self> {
        let inner = InnerVirtIODriver {
            virtio_driver_common: VirtIODriverCommonData::default(),
            driver_common: DriverCommonData::default(),
            kobj_common: KObjectCommonData::default(),
        };

        let id_table = VirtioDeviceId::new(
            virtio_drivers::transport::DeviceType::Network as u32,
            VIRTIO_VENDOR_ID.into(),
        );
        let result = VirtIONetDriver {
            inner: SpinLock::new(inner),
            kobj_state: LockedKObjectState::default(),
        };
        result.add_virtio_id(id_table);

        return Arc::new(result);
    }

    fn inner(&self) -> SpinLockGuard<'_, InnerVirtIODriver> {
        return self.inner.lock();
    }
}

#[derive(Debug)]
struct InnerVirtIODriver {
    virtio_driver_common: VirtIODriverCommonData,
    driver_common: DriverCommonData,
    kobj_common: KObjectCommonData,
}

impl VirtIODriver for VirtIONetDriver {
    fn probe(&self, device: &Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        log::debug!("VirtIONetDriver::probe()");
        let virtio_net_device = device
            .clone()
            .arc_any()
            .downcast::<VirtIONetDevice>()
            .map_err(|_| {
                error!(
                    "VirtIONetDriver::probe() failed: device is not a VirtIODevice. Device: '{:?}'",
                    device.name()
                );
                SystemError::EINVAL
            })?;

        let iface: Arc<VirtioInterface> =
            VirtioInterface::new(virtio_net_device.device_inner.clone());
        // 标识网络设备已经启动
        iface.set_net_state(NetDeivceState::__LINK_STATE_START);
        // 设置iface的父设备为virtio_net_device
        iface.set_dev_parent(Some(Arc::downgrade(&virtio_net_device) as Weak<dyn Device>));
        // 在sysfs中注册iface
        register_netdevice(iface.clone() as Arc<dyn Iface>)?;

        // 将virtio_net_device和iface关联起来
        virtio_net_device.set_iface(&iface);

        // 将网卡的接口信息注册到全局的网卡接口信息表中
        // NET_DEVICES
        //     .write_irqsave()
        //     .insert(iface.nic_id(), iface.clone());
        INIT_NET_NAMESPACE.add_device(iface.clone());
        iface
            .iface_common
            .set_net_namespace(INIT_NET_NAMESPACE.clone());
        INIT_NET_NAMESPACE.set_default_iface(iface.clone());

        virtio_irq_manager()
            .register_device(device.clone())
            .expect("Register virtio net irq failed");

        return Ok(());
    }

    fn virtio_id_table(&self) -> Vec<VirtioDeviceId> {
        self.inner().virtio_driver_common.id_table.clone()
    }

    fn add_virtio_id(&self, id: VirtioDeviceId) {
        self.inner().virtio_driver_common.id_table.push(id);
    }
}

impl Driver for VirtIONetDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new(VIRTIO_NET_BASENAME.to_string(), None))
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        let virtio_net_device = device
            .arc_any()
            .downcast::<VirtIONetDevice>()
            .expect("VirtIONetDriver::add_device() failed: device is not a VirtioInterface");

        self.inner()
            .driver_common
            .devices
            .push(virtio_net_device as Arc<dyn Device>);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let _virtio_net_device = device
            .clone()
            .arc_any()
            .downcast::<VirtIONetDevice>()
            .expect("VirtIONetDriver::delete_device() failed: device is not a VirtioInterface");

        let mut guard = self.inner();
        let index = guard
            .driver_common
            .devices
            .iter()
            .position(|dev| Arc::ptr_eq(device, dev))
            .expect("VirtIONetDriver::delete_device() failed: device not found");

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

impl KObject for VirtIONetDriver {
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
        VIRTIO_NET_BASENAME.to_string()
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
