use crate::arch::rand::rand;
use crate::driver::base::class::Class;
use crate::driver::base::device::bus::Bus;
use crate::driver::base::device::driver::Driver;
use crate::driver::base::device::{Device, DeviceCommonData, DeviceType, IdTable};
use crate::driver::base::kobject::{
    KObjType, KObject, KObjectCommonData, KObjectState, LockedKObjectState,
};
use crate::driver::base::kset::KSet;
use crate::filesystem::kernfs::KernFSInode;
use crate::init::initcall::INITCALL_DEVICE;
use crate::libs::rwlock::{RwLockReadGuard, RwLockWriteGuard};
use crate::libs::spinlock::{SpinLock, SpinLockGuard};
use crate::net::{generate_iface_id, NET_DEVICES};
use alloc::collections::VecDeque;
use alloc::fmt::Debug;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use smoltcp::phy::DeviceCapabilities;
use smoltcp::phy::{self, TxToken};
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr};
use system_error::SystemError;
use unified_init::macros::unified_init;

use super::bridge::BridgeEnableDevice;
use super::{register_netdevice, NetDeivceState, NetDeviceCommonData, Operstate};

use super::{Iface, IfaceCommon};

// const DEVICE_NAME: &str = "veth";

pub struct Veth {
    rx_queue: VecDeque<Vec<u8>>,
    peer: Option<Arc<SpinLock<Veth>>>,
}

impl Veth {
    pub fn new() -> Self {
        Veth {
            rx_queue: VecDeque::new(),
            peer: None,
        }
    }

    pub fn set_peer(&mut self, peer: Arc<SpinLock<Veth>>) {
        self.peer = Some(peer);
    }

    pub fn send_to_peer(&self, data: Vec<u8>) {
        if let Some(peer) = &self.peer {
            peer.lock().rx_queue.push_back(data);
        }
    }

    pub fn recv(&mut self) -> Option<Vec<u8>> {
        self.rx_queue.pop_front()
    }
}

#[derive(Clone)]
pub struct VethDriver {
    pub inner: Arc<SpinLock<Veth>>,
}

impl VethDriver {
    pub fn new_pair() -> (Self, Self) {
        let dev1 = Arc::new(SpinLock::new(Veth::new()));
        let dev2 = Arc::new(SpinLock::new(Veth::new()));

        dev1.lock().set_peer(dev2.clone());
        dev2.lock().set_peer(dev1.clone());

        (VethDriver { inner: dev1 }, VethDriver { inner: dev2 })
    }
}

pub struct VethTxToken {
    driver: VethDriver,
}

impl phy::TxToken for VethTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0; len];
        let result = f(&mut buf);
        self.driver.inner.lock().send_to_peer(buf);
        result
    }
}

pub struct VethRxToken {
    buffer: Vec<u8>,
}

impl phy::RxToken for VethRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.buffer)
    }
}

#[derive(Debug)]
struct VethDriverWarpper(UnsafeCell<VethDriver>);
unsafe impl Send for VethDriverWarpper {}
unsafe impl Sync for VethDriverWarpper {}

impl Deref for VethDriverWarpper {
    type Target = VethDriver;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.0.get() }
    }
}

impl DerefMut for VethDriverWarpper {
    fn deref_mut(&mut self) -> &mut Self::Target {
        unsafe { &mut *self.0.get() }
    }
}

impl VethDriverWarpper {
    #[allow(clippy::mut_from_ref)]
    #[allow(clippy::mut_from_ref)]
    fn force_get_mut(&self) -> &mut VethDriver {
        unsafe { &mut *self.0.get() }
    }
}

impl phy::Device for VethDriver {
    type RxToken<'a> = VethRxToken;
    type TxToken<'a> = VethTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut guard = self.inner.lock();
        guard.recv().map(|buf| {
            (
                VethRxToken { buffer: buf },
                VethTxToken {
                    driver: self.clone(),
                },
            )
        })
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(VethTxToken {
            driver: self.clone(),
        })
    }
}

#[cast_to([sync] Iface)]
#[cast_to([sync] Device)]
#[derive(Debug)]
pub struct VethInterface {
    name: String,
    driver: VethDriverWarpper,
    common: IfaceCommon,
    inner: SpinLock<InnerVethInterface>,
    locked_kobj_state: LockedKObjectState,
}

#[derive(Debug)]
pub struct InnerVethInterface {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
}

impl VethInterface {
    pub fn new(name: &str, driver: VethDriver) -> Arc<Self> {
        let iface_id = generate_iface_id();
        let mac = [
            0x02,
            0x00,
            0x00,
            0x00,
            (iface_id >> 8) as u8,
            iface_id as u8,
        ];
        let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
        let mut iface_config = smoltcp::iface::Config::new(hw_addr);
        iface_config.random_seed = rand() as u64;
        let mut iface = smoltcp::iface::Interface::new(
            iface_config,
            &mut driver.clone(),
            crate::time::Instant::now().into(),
        );
        iface.set_any_ip(true);

        let device = Arc::new(VethInterface {
            name: name.to_string(),
            driver: VethDriverWarpper(UnsafeCell::new(driver)),
            common: IfaceCommon::new(iface_id, true, iface),
            inner: SpinLock::new(InnerVethInterface {
                netdevice_common: NetDeviceCommonData::default(),
                device_common: DeviceCommonData::default(),
                kobj_common: KObjectCommonData::default(),
            }),
            locked_kobj_state: LockedKObjectState::default(),
        });

        device.set_net_state(NetDeivceState::__LINK_STATE_START);
        device.set_operstate(Operstate::IF_OPER_UP);
        NET_DEVICES
            .write_irqsave()
            .insert(device.nic_id(), device.clone());
        log::debug!(
            "VethInterface created, devices: {:?}",
            NET_DEVICES.read().keys()
        );
        register_netdevice(device.clone()).expect("register veth device failed");

        device
    }

    fn inner(&self) -> SpinLockGuard<InnerVethInterface> {
        self.inner.lock()
    }

    pub fn update_ip_addrs(&self, addr: IpAddress, cidr: u8) {
        let iface = &mut self.common.smol_iface.lock();
        let cidr = IpCidr::new(addr, cidr);
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs.push(cidr).expect("Push ipCidr failed: full");
        });

        // 默认路由
        iface.routes_mut().update(|routes_map| {
            routes_map
                .push(smoltcp::iface::Route {
                    cidr,
                    via_router: addr,
                    preferred_until: None,
                    expires_at: None,
                })
                .expect("Add default ipv4 route failed: full");
        });
    }
}

impl KObject for VethInterface {
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
        self.name.clone()
    }
    fn set_name(&self, _name: String) {}
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

impl Device for VethInterface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }
    fn id_table(&self) -> IdTable {
        IdTable::new(self.name.clone(), None)
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
        r
    }
    fn set_class(&self, class: Option<Weak<dyn Class>>) {
        self.inner().device_common.class = class;
    }
    fn driver(&self) -> Option<Arc<dyn Driver>> {
        let r = self.inner().device_common.driver.clone()?.upgrade();
        if r.is_none() {
            self.inner().device_common.driver = None;
        }
        r
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

impl Iface for VethInterface {
    fn common(&self) -> &IfaceCommon {
        &self.common
    }
    fn iface_name(&self) -> String {
        self.name.clone()
    }
    fn mac(&self) -> EthernetAddress {
        if let HardwareAddress::Ethernet(mac) = self.common.smol_iface.lock().hardware_addr() {
            mac
        } else {
            EthernetAddress([0, 0, 0, 0, 0, 0])
        }
    }
    fn poll(&self) {
        self.common.poll(self.driver.force_get_mut());
    }
    fn addr_assign_type(&self) -> u8 {
        self.inner().netdevice_common.addr_assign_type
    }
    fn net_device_type(&self) -> u16 {
        self.inner().netdevice_common.net_device_type = 1;
        self.inner().netdevice_common.net_device_type
    }
    fn net_state(&self) -> NetDeivceState {
        self.inner().netdevice_common.state
    }
    fn set_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state |= state;
    }
    fn operstate(&self) -> Operstate {
        self.inner().netdevice_common.operstate
    }
    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }
}

impl BridgeEnableDevice for VethInterface {
    fn bridge_transmit(&self, frame: &[u8]) {
        let driver = self.driver.force_get_mut();
        let token = VethTxToken {
            driver: driver.clone(),
        };
        token.consume(frame.len(), |buf| {
            buf.copy_from_slice(frame);
        });
    }
    fn name(&self) -> String {
        self.name.clone()
    }
    // fn bridge_receive(&self, frame: &[u8]) {
    //     let driver = self.driver.force_get_mut();
    //     let token = VethRxToken {
    //         buffer: frame.to_vec(),
    //     };
    // }
}

pub fn veth_probe() {
    let (drv0, drv1) = VethDriver::new_pair();
    VethInterface::new("veth0", drv0);
    VethInterface::new("veth1", drv1);
}

#[unified_init(INITCALL_DEVICE)]
pub fn veth_init() -> Result<(), SystemError> {
    veth_probe();
    log::info!("Veth pair initialized.");
    Ok(())
}

// use smoltcp::time::Instant;

// pub fn test_veth_loop() {
//     let mut dev_map = NET_DEVICES.write_irqsave();

//     let veth0 = dev_map
//         .iter()
//         .find(|(_, dev)| dev.iface_name() == "veth0")
//         .map(|(_, dev)| dev.clone())
//         .and_then(|dev| dev.as_any_ref().downcast_ref::<VethInterface>())
//         .expect("veth0 not found");

//     let veth1 = dev_map
//         .iter()
//         .find(|(_, dev)| dev.iface_name() == "veth1")
//         .map(|(_, dev)| dev.clone())
//         .and_then(|dev| dev.as_any_ref().downcast_ref::<VethInterface>())
//         .expect("veth1 not found");

//     // veth0 → veth1
//     {
//         let frame: &[u8] = &[
//             0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, // Destination MAC
//             0x11, 0x22, 0x33, 0x44, 0x55, 0x66, // Source MAC
//             0x08, 0x00, // Ethertype: IPv4
//             0xde, 0xad, 0xbe, 0xef, // Payload
//         ];
//         let tx = veth0.bridge_transmit(frame);

//         veth1.poll(); // 触发接收

//         veth1
//             .driver
//             .inner
//             .lock()
//             .rx_queue
//             .pop_front()
//             .map(|buf| {
//                 log::info!("[veth1 recv]: {:?}", &buf);
//             })
//             .unwrap_or_else(|| {
//                 log::warn!("[veth1 recv]: nothing received");
//             });
//     }

    // // veth1 → veth0
    // {
    //     let tx = veth1.bridge_transmit(Instant::now()).unwrap();
    //     let _ = tx.consume(32, |buf| {
    //         buf[..6].copy_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0x77]);
    //         buf[6..12].copy_from_slice(&[0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
    //         buf[12..14].copy_from_slice(&(0x0806u16.to_be_bytes())); // ethertype: ARP
    //         buf[14..].copy_from_slice(&[0xca, 0xfe, 0xba, 0xbe]);
    //     });

    //     veth0.poll();

    //     if let Some((rx, _)) = veth0.driver.force_get_mut().receive(Instant::now()) {
    //         rx.consume(|buf| {
    //             log::info!("[veth0 recv]: {:?}", &buf);
    //         });
    //     } else {
    //         log::warn!("[veth0 recv]: nothing received");
    //     }
    // }
// }
