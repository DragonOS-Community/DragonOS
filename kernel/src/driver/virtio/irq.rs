use alloc::sync::Arc;
use hashbrown::HashMap;
use system_error::SystemError;
use unified_init::macros::unified_init;

use crate::{
    driver::base::device::DeviceId,
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandler, IrqReturn},
        IrqNumber,
    },
    init::initcall::INITCALL_CORE,
    libs::{rwlock::RwLock, spinlock::SpinLock},
};

use super::VirtIODevice;

static mut VIRTIO_IRQ_MANAGER: Option<VirtIOIrqManager> = None;

#[inline(always)]
pub fn virtio_irq_manager() -> &'static VirtIOIrqManager {
    unsafe { VIRTIO_IRQ_MANAGER.as_ref().unwrap() }
}

pub struct VirtIOIrqManager {
    registration_lock: SpinLock<()>,
    map: RwLock<HashMap<Arc<DeviceId>, Arc<dyn VirtIODevice>>>,
    callbacks: RwLock<HashMap<Arc<DeviceId>, Arc<dyn VirtioIrqCallback>>>,
}

pub trait VirtioIrqCallback: Send + Sync {
    fn handle_irq(&self, irq: IrqNumber) -> Result<IrqReturn, SystemError>;
}

impl VirtIOIrqManager {
    fn new() -> Self {
        VirtIOIrqManager {
            registration_lock: SpinLock::new(()),
            map: RwLock::new(HashMap::new()),
            callbacks: RwLock::new(HashMap::new()),
        }
    }

    /// Registers a new device in the virtio interrupt request (IRQ) mapping.
    ///
    /// # Parameters
    ///
    /// - `device` - The device object implementing the `VirtIODevice` trait, wrapped in an `Arc` smart pointer.
    ///
    /// # Returns
    ///
    /// - If the device is successfully registered, returns `Ok(())`.
    /// - If the device ID already exists in the mapping, returns `Err(SystemError::EEXIST)`.
    pub fn register_device(&self, device: Arc<dyn VirtIODevice>) -> Result<(), SystemError> {
        let _guard = self.registration_lock.lock_irqsave();
        if self.callbacks.read_irqsave().contains_key(device.dev_id()) {
            return Err(SystemError::EEXIST);
        }
        let mut map = self.map.write_irqsave();

        if map.contains_key(device.dev_id()) {
            return Err(SystemError::EEXIST);
        }

        map.insert(device.dev_id().clone(), device);

        return Ok(());
    }

    /// Unregisters a device.
    ///
    /// This function removes the specified device from the internal mapping. The device is identified by its device ID.
    ///
    /// # Parameters
    ///
    /// - `dev_id` - The device ID of the device to be unregistered.
    #[allow(dead_code)]
    pub fn unregister_device(&self, dev_id: &Arc<DeviceId>) {
        let _guard = self.registration_lock.lock_irqsave();
        let mut map = self.map.write_irqsave();
        map.remove(dev_id);
    }

    pub fn register_callback(
        &self,
        dev_id: Arc<DeviceId>,
        callback: Arc<dyn VirtioIrqCallback>,
    ) -> Result<(), SystemError> {
        let _guard = self.registration_lock.lock_irqsave();
        if self.map.read_irqsave().contains_key(&dev_id) {
            return Err(SystemError::EEXIST);
        }

        let mut callbacks = self.callbacks.write_irqsave();
        if callbacks.contains_key(&dev_id) {
            return Err(SystemError::EEXIST);
        }
        callbacks.insert(dev_id, callback);
        Ok(())
    }

    pub fn unregister_callback(&self, dev_id: &Arc<DeviceId>) {
        let _guard = self.registration_lock.lock_irqsave();
        let mut callbacks = self.callbacks.write_irqsave();
        callbacks.remove(dev_id);
    }

    /// Looks up and returns the device with the specified device ID.
    ///
    /// # Parameters
    /// - `dev_id` - The device ID of the device to look up.
    ///
    /// # Returns
    /// - If the device is found, returns `Some` containing the device.
    /// - If no device is found, returns `None`.
    pub fn lookup_device(&self, dev_id: &Arc<DeviceId>) -> Option<Arc<dyn VirtIODevice>> {
        let map = self.map.read_irqsave();
        map.get(dev_id).cloned()
    }

    pub fn lookup_callback(&self, dev_id: &Arc<DeviceId>) -> Option<Arc<dyn VirtioIrqCallback>> {
        let callbacks = self.callbacks.read_irqsave();
        callbacks.get(dev_id).cloned()
    }
}

#[unified_init(INITCALL_CORE)]
fn init_virtio_irq_manager() -> Result<(), SystemError> {
    let manager = VirtIOIrqManager::new();
    unsafe {
        VIRTIO_IRQ_MANAGER = Some(manager);
    }
    return Ok(());
}

/// `DefaultVirtioIrqHandler` is the default interrupt handler for virtio devices.
///
/// This handler is invoked when a virtio device raises an interrupt.
///
/// It first checks whether the device ID exists, then attempts to look up the device
/// (or callback) associated with the device ID. If a device or callback is found, it
/// delegates interrupt handling to it. Otherwise it returns `IrqReturn::NotHandled`,
/// indicating that the interrupt was not handled.
#[derive(Debug)]
pub(super) struct DefaultVirtioIrqHandler;

impl IrqHandler for DefaultVirtioIrqHandler {
    fn handle(
        &self,
        irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        dev_id: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        let dev_id = dev_id.ok_or(SystemError::EINVAL)?;
        let dev_id = dev_id
            .arc_any()
            .downcast::<DeviceId>()
            .map_err(|_| SystemError::EINVAL)?;

        if let Some(dev) = virtio_irq_manager().lookup_device(&dev_id) {
            return dev.handle_irq(irq);
        } else if let Some(callback) = virtio_irq_manager().lookup_callback(&dev_id) {
            return callback.handle_irq(irq);
        } else {
            // No device or callback bound, so the interrupt cannot be handled
            // warn!("No device found for IRQ: {:?}", irq);
            return Ok(IrqReturn::NotHandled);
        }
    }
}
