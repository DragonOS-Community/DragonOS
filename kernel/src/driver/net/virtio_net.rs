use core::{
    any::Any,
    cell::UnsafeCell,
    fmt::Debug,
    ops::{Deref, DerefMut},
};

use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use log::{debug, error};
use smoltcp::{iface, phy, wire};
use unified_init::macros::unified_init;
use virtio_drivers::device::net::VirtIONet;

use super::{NetDeivceState, NetDevice, NetDeviceCommonData, Operstate};
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
        net::register_netdevice,
        virtio::{
            irq::virtio_irq_manager,
            sysfs::{virtio_bus, virtio_device_manager, virtio_driver_manager},
            transport::VirtIOTransport,
            virtio_impl::HalImpl,
            VirtIODevice, VirtIODeviceIndex, VirtIODriver, VirtIODriverCommonData, VirtioDeviceId,
            VIRTIO_VENDOR_ID,
        },
    },
    exception::{irqdesc::IrqReturn, IrqNumber},
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_POSTCORE,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    net::{generate_iface_id, net_core::poll_ifaces_try_lock_onetime, NET_DEVICES},
    time::Instant,
};
use system_error::SystemError;

static mut VIRTIO_NET_DRIVER: Option<Arc<VirtIONetDriver>> = None;

const VIRTIO_NET_BASENAME: &str = "virtio_net";

#[inline(always)]
#[allow(dead_code)]
fn virtio_net_driver() -> Arc<VirtIONetDriver> {
    unsafe { VIRTIO_NET_DRIVER.as_ref().unwrap().clone() }
}

/// virtio net device
#[derive(Debug)]
#[cast_to([sync] VirtIODevice)]
#[cast_to([sync] Device)]
pub struct VirtIONetDevice {
    dev_id: Arc<DeviceId>,
    inner: SpinLock<InnerVirtIONetDevice>,
    locked_kobj_state: LockedKObjectState,
}

unsafe impl Send for VirtIONetDevice {}
unsafe impl Sync for VirtIONetDevice {}

struct InnerVirtIONetDevice {
    device_inner: VirtIONicDeviceInner,
    name: Option<String>,
    virtio_index: Option<VirtIODeviceIndex>,
    kobj_common: KObjectCommonData,
    device_common: DeviceCommonData,
}

impl Debug for InnerVirtIONetDevice {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("InnerVirtIOBlkDevice").finish()
    }
}

impl VirtIONetDevice {
    pub fn new(transport: VirtIOTransport, dev_id: Arc<DeviceId>) -> Option<Arc<Self>> {
        // 设置中断
        if let Err(err) = transport.setup_irq(dev_id.clone()) {
            error!("VirtIONetDevice '{dev_id:?}' setup_irq failed: {:?}", err);
            return None;
        }

        let driver_net: VirtIONet<HalImpl, VirtIOTransport, 2> =
            match VirtIONet::<HalImpl, VirtIOTransport, 2>::new(transport, 4096) {
                Ok(net) => net,
                Err(_) => {
                    error!("VirtIONet init failed");
                    return None;
                }
            };
        let mac = wire::EthernetAddress::from_bytes(&driver_net.mac_address());
        debug!("VirtIONetDevice mac: {:?}", mac);
        let device_inner = VirtIONicDeviceInner::new(driver_net);
        device_inner.inner.lock_irqsave().enable_interrupts();
        let dev = Arc::new(Self {
            dev_id,
            inner: SpinLock::new(InnerVirtIONetDevice {
                device_inner,
                name: None,
                virtio_index: None,
                kobj_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
            }),
            locked_kobj_state: LockedKObjectState::default(),
        });

        // dev.set_driver(Some(Arc::downgrade(&virtio_net_driver()) as Weak<dyn Driver>));

        return Some(dev);
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIONetDevice> {
        return self.inner.lock();
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

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.locked_kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
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
}

impl VirtIODevice for VirtIONetDevice {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError> {
        if poll_ifaces_try_lock_onetime().is_err() {
            log::error!("virtio_net: try lock failed");
        }
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

pub struct VirtIoNetImpl {
    inner: VirtIONet<HalImpl, VirtIOTransport, 2>,
}

impl VirtIoNetImpl {
    const fn new(inner: VirtIONet<HalImpl, VirtIOTransport, 2>) -> Self {
        Self { inner }
    }
}

impl Deref for VirtIoNetImpl {
    type Target = VirtIONet<HalImpl, VirtIOTransport, 2>;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl DerefMut for VirtIoNetImpl {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

unsafe impl Send for VirtIoNetImpl {}
unsafe impl Sync for VirtIoNetImpl {}

#[derive(Debug)]
struct VirtIONicDeviceInnerWrapper(UnsafeCell<VirtIONicDeviceInner>);
unsafe impl Send for VirtIONicDeviceInnerWrapper {}
unsafe impl Sync for VirtIONicDeviceInnerWrapper {}

impl Deref for VirtIONicDeviceInnerWrapper {
    type Target = VirtIONicDeviceInner;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}
impl DerefMut for VirtIONicDeviceInnerWrapper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

#[allow(clippy::mut_from_ref)]
impl VirtIONicDeviceInnerWrapper {
    fn force_get_mut(&self) -> &mut <VirtIONicDeviceInnerWrapper as Deref>::Target {
        unsafe { &mut *self.0.get() }
    }
}

/// Virtio网络设备驱动(加锁)
pub struct VirtIONicDeviceInner {
    pub inner: Arc<SpinLock<VirtIoNetImpl>>,
}

impl Clone for VirtIONicDeviceInner {
    fn clone(&self) -> Self {
        return VirtIONicDeviceInner {
            inner: self.inner.clone(),
        };
    }
}

impl Debug for VirtIONicDeviceInner {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtIONicDriver").finish()
    }
}

#[cast_to([sync] NetDevice)]
#[cast_to([sync] Device)]
pub struct VirtioInterface {
    device_inner: VirtIONicDeviceInnerWrapper,
    iface_id: usize,
    iface_name: String,
    iface: SpinLock<iface::Interface>,
    inner: SpinLock<InnerVirtIOInterface>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug)]
struct InnerVirtIOInterface {
    kobj_common: KObjectCommonData,
    device_common: DeviceCommonData,
    netdevice_common: NetDeviceCommonData,
}

impl core::fmt::Debug for VirtioInterface {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VirtioInterface")
            .field("iface_id", &self.iface_id)
            .field("iface_name", &self.iface_name)
            .field("inner", &self.inner)
            .field("locked_kobj_state", &self.locked_kobj_state)
            .finish()
    }
}

impl VirtioInterface {
    pub fn new(mut device_inner: VirtIONicDeviceInner) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mut iface_config = iface::Config::new(wire::HardwareAddress::Ethernet(
            wire::EthernetAddress(device_inner.inner.lock().mac_address()),
        ));
        iface_config.random_seed = rand() as u64;

        let iface = iface::Interface::new(iface_config, &mut device_inner, Instant::now().into());

        let result = Arc::new(VirtioInterface {
            device_inner: VirtIONicDeviceInnerWrapper(UnsafeCell::new(device_inner)),
            iface_id,
            locked_kobj_state: LockedKObjectState::default(),
            iface: SpinLock::new(iface),
            iface_name: format!("eth{}", iface_id),
            inner: SpinLock::new(InnerVirtIOInterface {
                kobj_common: KObjectCommonData::default(),
                device_common: DeviceCommonData::default(),
                netdevice_common: NetDeviceCommonData::default(),
            }),
        });

        return result;
    }

    fn inner(&self) -> SpinLockGuard<InnerVirtIOInterface> {
        return self.inner.lock();
    }

    /// 获取网卡接口的名称
    #[allow(dead_code)]
    pub fn iface_name(&self) -> String {
        self.iface_name.clone()
    }
}

impl Drop for VirtioInterface {
    fn drop(&mut self) {
        // 从全局的网卡接口信息表中删除这个网卡的接口信息
        NET_DEVICES.write_irqsave().remove(&self.iface_id);
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
    pub fn new(driver_net: VirtIONet<HalImpl, VirtIOTransport, 2>) -> Self {
        let mut iface_config = iface::Config::new(wire::HardwareAddress::Ethernet(
            wire::EthernetAddress(driver_net.mac_address()),
        ));

        iface_config.random_seed = rand() as u64;

        let inner = Arc::new(SpinLock::new(VirtIoNetImpl::new(driver_net)));
        let result = VirtIONicDeviceInner { inner };
        return result;
    }
}

pub struct VirtioNetToken {
    driver: VirtIONicDeviceInner,
    rx_buffer: Option<virtio_drivers::device::net::RxBuffer>,
}

impl VirtioNetToken {
    pub fn new(
        driver: VirtIONicDeviceInner,
        rx_buffer: Option<virtio_drivers::device::net::RxBuffer>,
    ) -> Self {
        return Self { driver, rx_buffer };
    }
}

impl phy::Device for VirtIONicDeviceInner {
    type RxToken<'a>
        = VirtioNetToken
    where
        Self: 'a;
    type TxToken<'a>
        = VirtioNetToken
    where
        Self: 'a;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.inner.lock().receive() {
            Ok(buf) => Some((
                VirtioNetToken::new(self.clone(), Some(buf)),
                VirtioNetToken::new(self.clone(), None),
            )),
            Err(virtio_drivers::Error::NotReady) => None,
            Err(err) => panic!("VirtIO receive failed: {}", err),
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        // debug!("VirtioNet: transmit");
        if self.inner.lock_irqsave().can_send() {
            // debug!("VirtioNet: can send");
            return Some(VirtioNetToken::new(self.clone(), None));
        } else {
            // debug!("VirtioNet: can not send");
            return None;
        }
    }

    fn capabilities(&self) -> phy::DeviceCapabilities {
        let mut caps = phy::DeviceCapabilities::default();
        // 网卡的最大传输单元. 请与IP层的MTU进行区分。这个值应当是网卡的最大传输单元，而不是IP层的MTU。
        caps.max_transmission_unit = 2000;
        /*
           Maximum burst size, in terms of MTU.
           The network device is unable to send or receive bursts large than the value returned by this function.
           If None, there is no fixed limit on burst size, e.g. if network buffers are dynamically allocated.
        */
        caps.max_burst_size = Some(1);
        return caps;
    }
}

impl phy::TxToken for VirtioNetToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // // 为了线程安全，这里需要对VirtioNet进行加【写锁】，以保证对设备的互斥访问。
        let mut driver_net = self.driver.inner.lock();
        let mut tx_buf = driver_net.new_tx_buffer(len);
        let result = f(tx_buf.packet_mut());
        driver_net.send(tx_buf).expect("virtio_net send failed");
        return result;
    }
}

impl phy::RxToken for VirtioNetToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        // 为了线程安全，这里需要对VirtioNet进行加【写锁】，以保证对设备的互斥访问。
        let mut rx_buf = self.rx_buffer.unwrap();
        let result = f(rx_buf.packet_mut());
        self.driver
            .inner
            .lock()
            .recycle_rx_buffer(rx_buf)
            .expect("virtio_net recv failed");
        result
    }
}

/// @brief virtio-net 驱动的初始化与测试
pub fn virtio_net(
    transport: VirtIOTransport,
    dev_id: Arc<DeviceId>,
    dev_parent: Option<Arc<dyn Device>>,
) {
    let virtio_net_deivce = VirtIONetDevice::new(transport, dev_id);
    if let Some(virtio_net_deivce) = virtio_net_deivce {
        debug!("VirtIONetDevice '{:?}' created", virtio_net_deivce.dev_id);
        if let Some(dev_parent) = dev_parent {
            virtio_net_deivce.set_dev_parent(Some(Arc::downgrade(&dev_parent)));
        }
        virtio_device_manager()
            .device_add(virtio_net_deivce.clone() as Arc<dyn VirtIODevice>)
            .expect("Add virtio net failed");
    }
}

impl NetDevice for VirtioInterface {
    fn mac(&self) -> wire::EthernetAddress {
        let mac: [u8; 6] = self.device_inner.inner.lock().mac_address();
        return wire::EthernetAddress::from_bytes(&mac);
    }

    #[inline]
    fn nic_id(&self) -> usize {
        return self.iface_id;
    }

    #[inline]
    fn iface_name(&self) -> String {
        return self.iface_name.clone();
    }

    fn update_ip_addrs(&self, ip_addrs: &[wire::IpCidr]) -> Result<(), SystemError> {
        if ip_addrs.len() != 1 {
            return Err(SystemError::EINVAL);
        }

        self.iface.lock().update_ip_addrs(|addrs| {
            let dest = addrs.iter_mut().next();

            if let Some(dest) = dest {
                *dest = ip_addrs[0];
            } else {
                addrs
                    .push(ip_addrs[0])
                    .expect("Push wire::IpCidr failed: full");
            }
        });
        return Ok(());
    }

    fn poll(&self, sockets: &mut iface::SocketSet) -> Result<(), SystemError> {
        let timestamp: smoltcp::time::Instant = Instant::now().into();
        let mut guard = self.iface.lock();
        let poll_res = guard.poll(timestamp, self.device_inner.force_get_mut(), sockets);
        // todo: notify!!!
        // debug!("Virtio Interface poll:{poll_res}");
        if poll_res {
            return Ok(());
        }
        return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
    }

    #[inline(always)]
    fn inner_iface(&self) -> &SpinLock<iface::Interface> {
        return &self.iface;
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
        self.iface_name.clone()
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
        self.inner().kobj_common.kobj_type = ktype;
    }
}

#[unified_init(INITCALL_POSTCORE)]
fn virtio_net_driver_init() -> Result<(), SystemError> {
    let driver = VirtIONetDriver::new();
    virtio_driver_manager()
        .register(driver.clone() as Arc<dyn VirtIODriver>)
        .expect("Add virtio net driver failed");
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

    fn inner(&self) -> SpinLockGuard<InnerVirtIODriver> {
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
            VirtioInterface::new(virtio_net_device.inner().device_inner.clone());
        // 标识网络设备已经启动
        iface.set_net_state(NetDeivceState::__LINK_STATE_START);
        // 设置iface的父设备为virtio_net_device
        iface.set_dev_parent(Some(Arc::downgrade(&virtio_net_device) as Weak<dyn Device>));
        // 在sysfs中注册iface
        register_netdevice(iface.clone() as Arc<dyn NetDevice>)?;

        // 将网卡的接口信息注册到全局的网卡接口信息表中
        NET_DEVICES
            .write_irqsave()
            .insert(iface.nic_id(), iface.clone());

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
