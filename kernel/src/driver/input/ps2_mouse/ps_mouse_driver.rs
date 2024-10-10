use alloc::{
    string::ToString,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    arch::{io::PortIOArch, CurrentPortIOArch},
    driver::{
        base::{
            device::{bus::Bus, driver::Driver, Device, DeviceId, IdTable},
            kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
            kset::KSet,
        },
        input::serio::{
            serio_device::SerioDevice,
            serio_driver::{serio_driver_manager, SerioDriver},
        },
    },
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandleFlags, IrqHandler, IrqReturn},
        manage::irq_manager,
        IrqNumber,
    },
    filesystem::kernfs::KernFSInode,
    init::initcall::INITCALL_DEVICE,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::SpinLock,
    },
};

use super::ps_mouse_device::{ps2_mouse_device, Ps2MouseDevice};

const PS2_MOUSE_IRQ_NUM: IrqNumber = IrqNumber::new(0x2c);

#[no_mangle]
unsafe extern "C" fn ps2_mouse_driver_interrupt() {}

#[derive(Debug)]
struct Ps2MouseIrqHandler;

impl IrqHandler for Ps2MouseIrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        _dev_id: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        if let Some(psmouse_device) = ps2_mouse_device() {
            return Ok(ps2_mouse_driver()
                .interrupt(&(psmouse_device as Arc<dyn SerioDevice>), 0, 0)
                .map(|_| IrqReturn::Handled)
                .unwrap_or_else(|_| IrqReturn::NotHandled));
        } else {
            unsafe { CurrentPortIOArch::in8(0x60) };
            return Ok(IrqReturn::NotHandled);
        }
    }
}

static mut PS2_MOUSE_DRIVER: Option<Arc<Ps2MouseDriver>> = None;

#[allow(dead_code)]
pub fn ps2_mouse_driver() -> Arc<Ps2MouseDriver> {
    unsafe { PS2_MOUSE_DRIVER.clone().unwrap() }
}

#[derive(Debug)]
#[cast_to([sync] Driver)]
#[cast_to([sync] SerioDriver)]
pub struct Ps2MouseDriver {
    inner: SpinLock<InnerPs2MouseDriver>,
    kobj_state: LockedKObjectState,
}

impl Ps2MouseDriver {
    pub const NAME: &'static str = "psmouse";
    pub fn new() -> Arc<Self> {
        let r = Arc::new(Ps2MouseDriver {
            inner: SpinLock::new(InnerPs2MouseDriver {
                ktype: None,
                kset: None,
                parent: None,
                kernfs_inode: None,
                devices: Vec::new(),
                bus: None,
                self_ref: Weak::new(),
            }),
            kobj_state: LockedKObjectState::new(None),
        });

        r.inner.lock().self_ref = Arc::downgrade(&r);
        return r;
    }

    #[allow(dead_code)]
    pub fn process_packet(&self) {
        let guard = self.inner.lock();
        if guard.devices.is_empty() {
            return;
        }

        let device: Option<&Ps2MouseDevice> = guard.devices[0]
            .as_any_ref()
            .downcast_ref::<Ps2MouseDevice>();
        let _ = device.unwrap().process_packet();
    }
}

#[derive(Debug)]
pub struct InnerPs2MouseDriver {
    ktype: Option<&'static dyn KObjType>,
    kset: Option<Arc<KSet>>,
    parent: Option<Weak<dyn KObject>>,
    kernfs_inode: Option<Arc<KernFSInode>>,
    devices: Vec<Arc<dyn Device>>,
    bus: Option<Weak<dyn Bus>>,
    self_ref: Weak<Ps2MouseDriver>,
}

impl Driver for Ps2MouseDriver {
    fn id_table(&self) -> Option<IdTable> {
        Some(IdTable::new("psmouse".to_string(), None))
    }

    fn devices(&self) -> alloc::vec::Vec<Arc<dyn Device>> {
        self.inner.lock().devices.clone()
    }

    fn add_device(&self, device: Arc<dyn Device>) {
        let mut guard = self.inner.lock();
        // check if the device is already in the list
        if guard.devices.iter().any(|dev| Arc::ptr_eq(dev, &device)) {
            return;
        }

        guard.devices.push(device);
    }

    fn delete_device(&self, device: &Arc<dyn Device>) {
        let mut guard = self.inner.lock();
        guard.devices.retain(|dev| !Arc::ptr_eq(dev, device));
    }

    fn set_bus(&self, bus: Option<alloc::sync::Weak<dyn Bus>>) {
        self.inner.lock().bus = bus;
    }

    fn bus(&self) -> Option<Weak<dyn Bus>> {
        self.inner.lock().bus.clone()
    }
}

impl KObject for Ps2MouseDriver {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner.lock().kernfs_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner.lock().kernfs_inode.clone()
    }

    fn parent(&self) -> Option<alloc::sync::Weak<dyn KObject>> {
        self.inner.lock().parent.clone()
    }

    fn set_parent(&self, parent: Option<alloc::sync::Weak<dyn KObject>>) {
        self.inner.lock().parent = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner.lock().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner.lock().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        self.inner.lock().ktype
    }

    fn set_kobj_type(&self, ktype: Option<&'static dyn KObjType>) {
        self.inner.lock().ktype = ktype;
    }

    fn name(&self) -> alloc::string::String {
        Self::NAME.to_string()
    }

    fn set_name(&self, _name: alloc::string::String) {}

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

impl SerioDriver for Ps2MouseDriver {
    fn write_wakeup(
        &self,
        _device: &Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn interrupt(
        &self,
        device: &Arc<dyn SerioDevice>,
        _char: u8,
        _int: u8,
    ) -> Result<(), system_error::SystemError> {
        let device = device
            .clone()
            .arc_any()
            .downcast::<Ps2MouseDevice>()
            .map_err(|_| SystemError::EINVAL)?;
        device.process_packet()?;
        Ok(())
    }

    fn connect(&self, device: &Arc<dyn SerioDevice>) -> Result<(), system_error::SystemError> {
        let device = device
            .clone()
            .arc_any()
            .downcast::<Ps2MouseDevice>()
            .map_err(|_| SystemError::EINVAL)?;

        device.set_driver(Some(self.inner.lock_irqsave().self_ref.clone()));

        device.init()?;
        irq_manager().request_irq(
            PS2_MOUSE_IRQ_NUM,
            "psmouse".to_string(),
            &Ps2MouseIrqHandler,
            IrqHandleFlags::IRQF_SHARED | IrqHandleFlags::IRQF_TRIGGER_RISING,
            Some(DeviceId::new(Some(Self::NAME), None).unwrap()),
        )?;
        return Ok(());
    }

    fn reconnect(&self, _device: &Arc<dyn SerioDevice>) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn fast_reconnect(
        &self,
        _device: &Arc<dyn SerioDevice>,
    ) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn disconnect(&self, _device: &Arc<dyn SerioDevice>) -> Result<(), system_error::SystemError> {
        todo!()
    }

    fn cleanup(&self, _device: &Arc<dyn SerioDevice>) -> Result<(), system_error::SystemError> {
        todo!()
    }
}

#[unified_init(INITCALL_DEVICE)]
fn ps2_mouse_driver_init() -> Result<(), SystemError> {
    kdebug!("Ps2_mouse_drive initializing...");
    let driver = Ps2MouseDriver::new();
    serio_driver_manager().register(driver.clone())?;
    unsafe { PS2_MOUSE_DRIVER = Some(driver) };
    kdebug!("Ps2_mouse_drive initialized!");
    return Ok(());
}
