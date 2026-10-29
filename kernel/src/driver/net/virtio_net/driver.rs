use super::*;

static mut VIRTIO_NET_DRIVER: Option<Arc<VirtIONetDriver>> = None;

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

        // IRQ dispatch is installed first, but the handler cannot reach this
        // interface until the common netdevice transaction commits.
        virtio_irq_manager().register_device(device.clone())?;
        if let Err(error) = register_netdevice(&INIT_NET_NAMESPACE, iface.clone() as Arc<dyn Iface>)
        {
            virtio_irq_manager().unregister_device(device.dev_id());
            return Err(error);
        }

        virtio_net_device.set_iface(&iface);
        // Reconcile an interrupt acknowledged in the short committed-but-not-
        // linked window. An idle poll is harmless and avoids lost edge events.
        if let Some(napi) = iface.napi_struct() {
            crate::driver::net::napi::napi_schedule(napi);
        }

        INIT_NET_NAMESPACE.set_default_iface(iface.clone());

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
