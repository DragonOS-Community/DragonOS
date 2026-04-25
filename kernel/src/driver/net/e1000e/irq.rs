use alloc::sync::Arc;
use hashbrown::HashMap;
use lazy_static::lazy_static;
use system_error::SystemError;

use crate::{
    driver::{
        base::device::DeviceId,
        net::napi::{napi_schedule, NapiStruct},
    },
    exception::{
        irqdata::IrqHandlerData,
        irqdesc::{IrqHandler, IrqReturn},
        IrqNumber,
    },
    libs::rwlock::RwLock,
};

lazy_static! {
    static ref E1000E_IRQ_MANAGER: E1000EIrqManager = E1000EIrqManager::new();
}

#[inline(always)]
pub fn e1000e_irq_manager() -> &'static E1000EIrqManager {
    &E1000E_IRQ_MANAGER
}

/// e1000e 驱动域内的 IRQ -> NAPI 映射表。
///
/// 语义：在 **hardirq** 上下文仅做 `napi_schedule()`，不扫描 netns.device_list。
pub struct E1000EIrqManager {
    map: RwLock<HashMap<Arc<DeviceId>, Arc<NapiStruct>>>,
}

impl E1000EIrqManager {
    fn new() -> Self {
        Self {
            map: RwLock::new(HashMap::new()),
        }
    }

    pub fn register_napi(
        &self,
        dev_id: Arc<DeviceId>,
        napi: Arc<NapiStruct>,
    ) -> Result<(), SystemError> {
        let mut map = self.map.write_irqsave();
        if map.contains_key(&dev_id) {
            return Err(SystemError::EEXIST);
        }
        map.insert(dev_id, napi);
        Ok(())
    }

    pub fn lookup_napi(&self, dev_id: &Arc<DeviceId>) -> Option<Arc<NapiStruct>> {
        let map = self.map.read_irqsave();
        map.get(dev_id).cloned()
    }
}

/// e1000e 的默认 IRQ handler：通过 PCI irq 子系统传入的 `DeviceId` 查表并 schedule NAPI。
#[derive(Debug)]
pub struct DefaultE1000EIrqHandler;

impl IrqHandler for DefaultE1000EIrqHandler {
    fn handle(
        &self,
        _irq: IrqNumber,
        _static_data: Option<&dyn IrqHandlerData>,
        dev_id: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError> {
        let dev_id = dev_id.ok_or(SystemError::EINVAL)?;
        let dev_id = dev_id
            .arc_any()
            .downcast::<DeviceId>()
            .map_err(|_| SystemError::EINVAL)?;

        if let Some(napi) = e1000e_irq_manager().lookup_napi(&dev_id) {
            napi_schedule(napi);
            return Ok(IrqReturn::Handled);
        }

        Ok(IrqReturn::NotHandled)
    }
}
