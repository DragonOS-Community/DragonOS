use core::any::Any;

use alloc::sync::Arc;
use system_error::SystemError;

use crate::exception::{irqdesc::IrqReturn, IrqNumber};

use super::base::device::DeviceId;

pub(super) mod irq;
pub mod transport_pci;
pub mod virtio;
pub mod virtio_impl;

pub trait VirtIODevice: Send + Sync + Any {
    fn handle_irq(&self, _irq: IrqNumber) -> Result<IrqReturn, SystemError>;

    fn dev_id(&self) -> &Arc<DeviceId>;
}
