use alloc::{string::ToString, sync::Arc};

use virtio_drivers::transport::Transport;

use crate::{
    driver::{
        base::device::DeviceId,
        pci::{
            pci::{PciDeviceStructure, PciError},
            pci_irq::{IrqCommonMsg, IrqSpecificMsg, PciInterrupt, PciIrqError, PciIrqMsg, IRQ},
        },
    },
    exception::IrqNumber,
};

use super::{
    irq::DefaultVirtioIrqHandler, transport_mmio::VirtIOMmioTransport, transport_pci::PciTransport,
};

pub enum VirtIOTransport {
    Pci(PciTransport),
    Mmio(VirtIOMmioTransport),
}

impl VirtIOTransport {
    pub fn irq(&self) -> IrqNumber {
        match self {
            VirtIOTransport::Pci(transport) => transport.irq(),
            VirtIOTransport::Mmio(transport) => IrqNumber::new(transport.irq().data()),
        }
    }

    /// 设置中断
    pub fn setup_irq(&self, dev_id: Arc<DeviceId>) -> Result<(), PciError> {
        if let VirtIOTransport::Pci(transport) = self {
            let standard_device = transport.pci_device().as_standard_device().unwrap();
            standard_device
                .irq_init(IRQ::PCI_IRQ_MSIX | IRQ::PCI_IRQ_MSI)
                .ok_or(PciError::PciIrqError(PciIrqError::IrqNotInited))?;
            // 中断相关信息
            let msg = PciIrqMsg {
                irq_common_message: IrqCommonMsg::init_from(
                    0,
                    "Virtio_IRQ".to_string(),
                    &DefaultVirtioIrqHandler,
                    dev_id,
                ),
                irq_specific_message: IrqSpecificMsg::msi_default(),
            };
            standard_device.irq_install(msg)?;
            standard_device.irq_enable(true)?;
        }
        return Ok(());
    }
}

impl core::fmt::Debug for VirtIOTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            VirtIOTransport::Pci(_) => write!(f, "VirtIOTransport::Pci"),
            VirtIOTransport::Mmio(_) => write!(f, "VirtIOTransport::Mmio"),
        }
    }
}

impl Transport for VirtIOTransport {
    #[inline(always)]
    fn finish_init(&mut self) {
        match self {
            VirtIOTransport::Pci(transport) => transport.finish_init(),
            VirtIOTransport::Mmio(transport) => transport.finish_init(),
        }
    }

    #[inline(always)]
    fn device_type(&self) -> virtio_drivers::transport::DeviceType {
        match self {
            VirtIOTransport::Pci(transport) => transport.device_type(),
            VirtIOTransport::Mmio(transport) => transport.device_type(),
        }
    }

    #[inline(always)]
    fn read_device_features(&mut self) -> u64 {
        match self {
            VirtIOTransport::Pci(transport) => transport.read_device_features(),
            VirtIOTransport::Mmio(transport) => transport.read_device_features(),
        }
    }

    #[inline(always)]
    fn write_driver_features(&mut self, driver_features: u64) {
        match self {
            VirtIOTransport::Pci(transport) => transport.write_driver_features(driver_features),
            VirtIOTransport::Mmio(transport) => transport.write_driver_features(driver_features),
        }
    }

    #[inline(always)]
    fn max_queue_size(&mut self, queue: u16) -> u32 {
        match self {
            VirtIOTransport::Pci(transport) => transport.max_queue_size(queue),
            VirtIOTransport::Mmio(transport) => transport.max_queue_size(queue),
        }
    }

    #[inline(always)]
    fn notify(&mut self, queue: u16) {
        match self {
            VirtIOTransport::Pci(transport) => transport.notify(queue),
            VirtIOTransport::Mmio(transport) => transport.notify(queue),
        }
    }

    #[inline(always)]
    fn get_status(&self) -> virtio_drivers::transport::DeviceStatus {
        match self {
            VirtIOTransport::Pci(transport) => transport.get_status(),
            VirtIOTransport::Mmio(transport) => transport.get_status(),
        }
    }

    #[inline(always)]
    fn set_status(&mut self, status: virtio_drivers::transport::DeviceStatus) {
        match self {
            VirtIOTransport::Pci(transport) => transport.set_status(status),
            VirtIOTransport::Mmio(transport) => transport.set_status(status),
        }
    }

    #[inline(always)]
    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        match self {
            VirtIOTransport::Pci(transport) => transport.set_guest_page_size(guest_page_size),
            VirtIOTransport::Mmio(transport) => transport.set_guest_page_size(guest_page_size),
        }
    }

    #[inline(always)]
    fn requires_legacy_layout(&self) -> bool {
        match self {
            VirtIOTransport::Pci(transport) => transport.requires_legacy_layout(),
            VirtIOTransport::Mmio(transport) => transport.requires_legacy_layout(),
        }
    }

    #[inline(always)]
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: virtio_drivers::PhysAddr,
        driver_area: virtio_drivers::PhysAddr,
        device_area: virtio_drivers::PhysAddr,
    ) {
        match self {
            VirtIOTransport::Pci(transport) => {
                transport.queue_set(queue, size, descriptors, driver_area, device_area)
            }
            VirtIOTransport::Mmio(transport) => {
                transport.queue_set(queue, size, descriptors, driver_area, device_area)
            }
        }
    }

    #[inline(always)]
    fn queue_unset(&mut self, queue: u16) {
        match self {
            VirtIOTransport::Pci(transport) => transport.queue_unset(queue),
            VirtIOTransport::Mmio(transport) => transport.queue_unset(queue),
        }
    }

    #[inline(always)]
    fn queue_used(&mut self, queue: u16) -> bool {
        match self {
            VirtIOTransport::Pci(transport) => transport.queue_used(queue),
            VirtIOTransport::Mmio(transport) => transport.queue_used(queue),
        }
    }

    #[inline(always)]
    fn ack_interrupt(&mut self) -> bool {
        match self {
            VirtIOTransport::Pci(transport) => transport.ack_interrupt(),
            VirtIOTransport::Mmio(transport) => transport.ack_interrupt(),
        }
    }

    #[inline(always)]
    fn config_space<T: 'static>(&self) -> virtio_drivers::Result<core::ptr::NonNull<T>> {
        match self {
            VirtIOTransport::Pci(transport) => transport.config_space(),
            VirtIOTransport::Mmio(transport) => transport.config_space(),
        }
    }
}
