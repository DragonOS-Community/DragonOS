use super::*;

#[derive(Clone)]
struct BridgeNetDriver {
    bridge: Arc<BridgeDriver>,
}

struct BridgeRxToken(Vec<u8>);

impl RxToken for BridgeRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

struct BridgeTxToken(BridgeNetDriver);

impl phy::TxToken for BridgeTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut frame = vec![0; len];
        let result = f(&mut frame);
        let _ = self.0.bridge.local_transmit(&frame);
        result
    }
}

impl phy::Device for BridgeNetDriver {
    type RxToken<'a> = BridgeRxToken;
    type TxToken<'a> = BridgeTxToken;

    fn capabilities(&self) -> DeviceCapabilities {
        let mut caps = DeviceCapabilities::default();
        // smoltcp retains this as the ceiling for later set_ip_mtu calls.
        caps.max_transmission_unit = crate::driver::net::ETHERNET_MAX_IP_MTU + 14;
        caps.medium = phy::Medium::Ethernet;
        caps
    }

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        self.bridge
            .local_rx_queue
            .lock()
            .pop_front()
            .map(|frame| (BridgeRxToken(frame), BridgeTxToken(self.clone())))
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(BridgeTxToken(self.clone()))
    }
}

#[derive(Debug, Default)]
struct BridgeDeviceData {
    netdevice_common: NetDeviceCommonData,
    device_common: DeviceCommonData,
    kobj_common: KObjectCommonData,
}

/// A bridge is a first-class namespace-local Ethernet interface. Its driver
/// owns the port/FDB state; this object supplies the host's local IP endpoint.
#[cast_to([sync] Iface)]
#[cast_to([sync] device::Device)]
#[derive(Debug)]
pub struct BridgeIface {
    driver: Arc<BridgeDriver>,
    common: IfaceCommon,
    mac_address: SpinLock<EthernetAddress>,
    inner: SpinLock<BridgeDeviceData>,
    locked_kobj_state: LockedKObjectState,
}

/// A prevalidated, allocation-free-to-commit bridge port removal. Callers
/// holding RTNL may chain `staged_fib` into another bridge or route mutation
/// before removing any port.
pub(crate) struct PreparedBridgePortRemoval<'rtnl> {
    bridge: Arc<BridgeDriver>,
    devices: Vec<(BridgePortId, Arc<dyn BridgeEnableDevice>)>,
    mtu: Option<crate::net::link::PreparedAutoMtu<'rtnl>>,
}

impl PreparedBridgePortRemoval<'_> {
    pub(crate) fn staged_fib(&self) -> Option<crate::net::link::StagedLinkFib<'_>> {
        self.mtu.as_ref().and_then(|mtu| mtu.staged_fib())
    }

    pub(crate) fn commit(self) -> Option<crate::net::link::LinkMutationCommit> {
        self.bridge.commit_remove_devices(self.devices);
        self.mtu.map(|mtu| mtu.commit())
    }
}

impl BridgeIface {
    pub fn new(name: &str) -> Arc<Self> {
        let id = generate_iface_id();
        let mac = EthernetAddress([
            0x02,
            (id >> 32) as u8,
            (id >> 24) as u8,
            (id >> 16) as u8,
            (id >> 8) as u8,
            id as u8,
        ]);
        let driver = BridgeDriver::new(name);
        driver.active.store(false, Ordering::SeqCst);
        driver.inner.lock().local_mac = Some(mac);
        let mut phy = BridgeNetDriver {
            bridge: driver.clone(),
        };
        let mut config = smoltcp::iface::Config::new(HardwareAddress::Ethernet(mac));
        config.random_seed = rand() as u64;
        let mut stack = smoltcp::iface::Interface::new(config, &mut phy, Instant::now().into());
        stack.set_any_ip(true);
        let iface = Arc::new(Self {
            driver: driver.clone(),
            common: IfaceCommon::new(
                id,
                InterfaceType::ETHER,
                name.into(),
                BRIDGE_MTU,
                InterfaceFlags::BROADCAST | InterfaceFlags::MULTICAST,
                stack,
            ),
            mac_address: SpinLock::new(mac),
            inner: SpinLock::new(BridgeDeviceData::default()),
            locked_kobj_state: LockedKObjectState::default(),
        });
        iface.set_net_state(NetDeivceState::__LINK_STATE_NOCARRIER);
        iface.set_operstate(Operstate::IF_OPER_DOWN);
        *iface.common.napi_struct.write() = Some(NapiStruct::new(iface.clone(), 10));
        *driver.local_iface.lock() = Arc::downgrade(&iface);
        iface
    }

    fn prepare_auto_mtu<'rtnl>(
        &self,
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        netns: &Arc<NetNamespace>,
        candidate: usize,
    ) -> Result<Option<crate::net::link::PreparedAutoMtu<'rtnl>>, SystemError> {
        self.prepare_auto_mtu_from(rtnl, netns, candidate, None)
    }

    fn prepare_auto_mtu_from<'rtnl>(
        &self,
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        netns: &Arc<NetNamespace>,
        candidate: usize,
        staged_before: Option<crate::net::link::StagedLinkFib<'_>>,
    ) -> Result<Option<crate::net::link::PreparedAutoMtu<'rtnl>>, SystemError> {
        if self.driver.mtu_set_by_user() || candidate == self.mtu() {
            return Ok(None);
        }
        let iface: Arc<dyn Iface> = self.driver.local_iface().ok_or(SystemError::ENODEV)?;
        crate::net::link::prepare_auto_mtu(rtnl, netns, iface, candidate, staged_before).map(Some)
    }

    pub(crate) fn prepare_port_mtu_change<'rtnl>(
        &self,
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        netns: &Arc<NetNamespace>,
        device: &VethInterface,
        new_mtu: usize,
        staged_before: Option<crate::net::link::StagedLinkFib<'_>>,
    ) -> Result<Option<crate::net::link::PreparedAutoMtu<'rtnl>>, SystemError> {
        if self.driver.mtu_set_by_user() {
            return Ok(None);
        }
        let port_id = device.common_bridge_data().ok_or(SystemError::ENODEV)?.id;
        let bridge = self.driver.inner.lock();
        if !bridge.ports.contains_key(&port_id) {
            return Err(SystemError::ENODEV);
        }
        let candidate = bridge
            .ports
            .values()
            .map(|port| {
                if port.id == port_id {
                    new_mtu
                } else {
                    port.bridge_enable.mtu()
                }
            })
            .min()
            .unwrap_or(BRIDGE_MTU);
        drop(bridge);
        self.prepare_auto_mtu_from(rtnl, netns, candidate, staged_before)
    }

    pub(crate) fn add_port(
        &self,
        rtnl: &crate::net::rtnl::RtnlGuard,
        netns: &Arc<NetNamespace>,
        device: Arc<VethInterface>,
    ) -> Result<Option<crate::net::link::LinkMutationCommit>, SystemError> {
        let candidate = self
            .driver
            .inner
            .lock()
            .ports
            .values()
            .map(|port| port.bridge_enable.mtu())
            .chain(core::iter::once(device.mtu()))
            .min()
            .unwrap_or(BRIDGE_MTU);
        let mtu = self.prepare_auto_mtu(rtnl, netns, candidate)?;
        self.driver
            .try_add_device(device as Arc<dyn BridgeEnableDevice>)?;
        Ok(mtu.map(|plan| plan.commit()))
    }

    pub(crate) fn remove_port(
        &self,
        rtnl: &crate::net::rtnl::RtnlGuard,
        netns: &Arc<NetNamespace>,
        device: Arc<VethInterface>,
    ) -> Result<Option<crate::net::link::LinkMutationCommit>, SystemError> {
        Ok(self
            .prepare_remove_ports(rtnl, netns, core::slice::from_ref(&device), None)?
            .commit())
    }

    /// Prepare all removals from this bridge against one final MTU. The
    /// preceding staged FIB is used when a different bridge (or veth port)
    /// has already prepared a route/address change in the same namespace.
    pub(crate) fn prepare_remove_ports<'rtnl>(
        &self,
        rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
        netns: &Arc<NetNamespace>,
        devices: &[Arc<VethInterface>],
        staged_before: Option<crate::net::link::StagedLinkFib<'_>>,
    ) -> Result<PreparedBridgePortRemoval<'rtnl>, SystemError> {
        let mut prepared = Vec::new();
        prepared
            .try_reserve(devices.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for device in devices {
            let data = device.common_bridge_data().ok_or(SystemError::EINVAL)?;
            let owner = data
                .bridge_driver_ref
                .upgrade()
                .ok_or(SystemError::ENODEV)?;
            if !Arc::ptr_eq(&self.driver, &owner) || prepared.iter().any(|(id, _)| *id == data.id) {
                return Err(SystemError::EINVAL);
            }
            let port_device: Arc<dyn BridgeEnableDevice> = device.clone();
            prepared.push((data.id, port_device));
        }
        // Query veth bridge data before taking the bridge lock: ingress can
        // obtain the veth lock and then enter the bridge/FDB lock.
        let bridge = self.driver.inner.lock();
        for (id, device) in &prepared {
            let port = bridge.ports.get(id).ok_or(SystemError::ENODEV)?;
            if !Arc::ptr_eq(&port.bridge_enable, device) {
                return Err(SystemError::EINVAL);
            }
        }
        let candidate = bridge
            .ports
            .values()
            .filter(|port| !prepared.iter().any(|(id, _)| *id == port.id))
            .map(|port| port.bridge_enable.mtu())
            .min()
            .unwrap_or(BRIDGE_MTU);
        drop(bridge);
        let mtu = self.prepare_auto_mtu_from(rtnl, netns, candidate, staged_before)?;
        Ok(PreparedBridgePortRemoval {
            bridge: self.driver.clone(),
            devices: prepared,
            mtu,
        })
    }

    /// A bridge being deleted has no future automatic MTU to publish. Still
    /// prevalidate and reserve all detach work before mutating any port.
    pub(crate) fn prepare_detach_all_ports<'rtnl>(
        &self,
        _rtnl: &'rtnl crate::net::rtnl::RtnlGuard,
    ) -> Result<PreparedBridgePortRemoval<'rtnl>, SystemError> {
        let bridge = self.driver.inner.lock();
        let mut devices = Vec::new();
        devices
            .try_reserve(bridge.ports.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for port in bridge.ports.values() {
            devices.push((port.id, port.bridge_enable.clone()));
        }
        Ok(PreparedBridgePortRemoval {
            bridge: self.driver.clone(),
            devices,
            mtu: None,
        })
    }

    pub fn detach_all_ports(&self) -> Result<(), SystemError> {
        self.driver.detach_all_ports()
    }

    /// Teardown of an already-destroyed netns has no namespace/FIB to update.
    pub(crate) fn detach_port_without_netns(
        &self,
        device: Arc<VethInterface>,
    ) -> Result<(), SystemError> {
        self.driver
            .try_remove_device(device as Arc<dyn BridgeEnableDevice>)
    }

    /// A peer whose own netns has already died must be removed without a
    /// fallible FIB snapshot. Removing a port cannot lower the bridge MTU
    /// below an IP minimum except when the last port restores 1500, so its
    /// existing addresses and routes remain valid.
    pub(crate) fn detach_port_for_netns_teardown(
        &self,
        device: Arc<VethInterface>,
    ) -> Result<(), SystemError> {
        self.driver
            .try_remove_device(device as Arc<dyn BridgeEnableDevice>)?;
        if !self.driver.mtu_set_by_user() {
            let candidate = self
                .driver
                .inner
                .lock()
                .ports
                .values()
                .map(|port| port.bridge_enable.mtu())
                .min()
                .unwrap_or(BRIDGE_MTU);
            if candidate != self.mtu() {
                self.common
                    .smol_iface
                    .lock()
                    .set_ip_mtu(self.stack_mtu(candidate))
                    .expect("attached port MTU must fit the bridge interface");
                self.common.set_mtu(candidate);
                let iface = self
                    .driver
                    .local_iface()
                    .expect("live bridge has a local iface");
                crate::net::socket::netlink::notify_link_change(&(iface as Arc<dyn Iface>));
            }
        }
        Ok(())
    }

    pub fn bridge_driver(&self) -> Arc<BridgeDriver> {
        self.driver.clone()
    }

    pub fn set_mac(&self, mac: EthernetAddress) -> Result<(), SystemError> {
        let bytes = mac.as_bytes();
        if bytes[0] & 1 != 0 || bytes.iter().all(|byte| *byte == 0) {
            return Err(SystemError::EINVAL);
        }
        let napi = self.napi_struct();
        if let Some(napi) = napi.as_ref() {
            crate::driver::net::napi::napi_pause_and_wait(napi);
        }
        self.common
            .smol_iface
            .lock()
            .set_hardware_addr(HardwareAddress::Ethernet(mac));
        let mut bridge = self.driver.inner.lock();
        bridge.local_mac = Some(mac);
        bridge.forget_macs(mac, mac);
        drop(bridge);
        *self.mac_address.lock() = mac;
        if let Some(napi) = napi {
            crate::driver::net::napi::napi_resume(napi);
        }
        Ok(())
    }

    fn inner(&self) -> SpinLockGuard<'_, BridgeDeviceData> {
        self.inner.lock()
    }
}

impl KObject for BridgeIface {
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
        self.common.name()
    }
    fn set_name(&self, name: String) {
        self.common.set_name(name);
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

impl device::Device for BridgeIface {
    fn dev_type(&self) -> DeviceType {
        DeviceType::Net
    }
    fn id_table(&self) -> IdTable {
        IdTable::new(self.common.name(), None)
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
        let weak = self.inner().device_common.driver.clone()?;
        weak.upgrade()
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
    fn dev_parent(&self) -> Option<Weak<dyn device::Device>> {
        self.inner().device_common.get_parent_weak_or_clear()
    }
    fn set_dev_parent(&self, parent: Option<Weak<dyn device::Device>>) {
        self.inner().device_common.parent = parent;
    }
}

impl Iface for BridgeIface {
    fn destroy_on_netns_exit(&self) -> bool {
        true
    }
    fn common(&self) -> &IfaceCommon {
        &self.common
    }
    fn iface_name(&self) -> String {
        self.common.name()
    }
    fn mac(&self) -> EthernetAddress {
        *self.mac_address.lock()
    }
    fn poll(&self) -> bool {
        let mut driver = BridgeNetDriver {
            bridge: self.driver.clone(),
        };
        match self.common.poll_scope() {
            IfacePollScope::None => false,
            IfacePollScope::LocalOnly => self.common.poll(&mut driver),
            IfacePollScope::Full => {
                let result = self.poll_napi(64);
                result.work_done != 0 || result.poll_again
            }
        }
    }
    fn poll_napi(&self, budget: usize) -> crate::driver::net::napi::NapiPollResult {
        let mut driver = BridgeNetDriver {
            bridge: self.driver.clone(),
        };
        match self.common.poll_scope() {
            IfacePollScope::None => crate::driver::net::napi::NapiPollResult::idle(),
            IfacePollScope::LocalOnly => self.common.poll_napi(&mut driver, budget),
            IfacePollScope::Full => self.common.poll_napi_routed_ingress(&mut driver, budget),
        }
    }
    fn raw_transmit(&self, frame: &[u8]) -> Result<(), SystemError> {
        self.driver.local_transmit(frame)
    }
    fn addr_assign_type(&self) -> u8 {
        self.inner().netdevice_common.addr_assign_type
    }
    fn net_device_type(&self) -> u16 {
        1
    }
    fn net_state(&self) -> NetDeivceState {
        self.inner().netdevice_common.state
    }
    fn set_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state |= state;
        if state.contains(NetDeivceState::__LINK_STATE_START) {
            self.driver.update_carrier_from_ports();
        }
    }
    fn clear_net_state(&self, state: NetDeivceState) {
        self.inner().netdevice_common.state &= !state;
    }
    fn operstate(&self) -> Operstate {
        self.inner().netdevice_common.operstate
    }
    fn set_operstate(&self, state: Operstate) {
        self.inner().netdevice_common.operstate = state;
    }
    fn mtu(&self) -> usize {
        self.common.mtu()
    }
    fn mtu_bounds(&self) -> MtuBounds {
        MtuBounds {
            min: 68,
            max: crate::driver::net::ETHERNET_MAX_IP_MTU,
        }
    }
    fn set_net_namespace(&self, ns: Arc<NetNamespace>) -> Result<(), SystemError> {
        self.common.set_net_namespace(ns.clone())?;
        self.driver.set_netns(&ns);
        Ok(())
    }
    fn clear_net_namespace(&self) {
        self.driver.clear_netns();
        self.common.clear_net_namespace();
    }
    fn begin_admin_down(&self) {
        self.driver.active.store(false, Ordering::SeqCst);
    }
    fn quiesce_admin_down(&self) {
        self.driver.wait_idle();
        self.driver.local_rx_queue.lock().clear();
    }
    fn publish_admin_state(&self, is_up: bool) {
        self.driver.active.store(is_up, Ordering::SeqCst);
        self.driver.update_carrier_from_ports();
    }
}
