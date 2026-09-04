use alloc::{string::ToString, sync::Arc};

use virtio_drivers::transport::Transport;

use crate::{
    arch::msi::arch_pci_msi_vector_release,
    driver::{
        base::device::DeviceId,
        pci::pci_irq::IrqType,
        pci::{
            pci::{
                PciBarSubresourceGuard, PciDeviceStructure, PciDeviceStructureGeneralDevice,
                PciError,
            },
            pci_irq::{IrqCommonMsg, IrqSpecificMsg, PciInterrupt, PciIrqMsg, IRQ},
        },
    },
    exception::{irqdesc::IrqHandleFlags, manage::irq_manager, IrqNumber},
    mm::PhysAddr,
};

use super::{
    irq::DefaultVirtioIrqHandler, transport_mmio::VirtIOMmioTransport, transport_pci::PciTransport,
};

/// An MMIO interrupt request which must not be installed until the concrete
/// VirtIO device has been fully constructed.
pub(crate) struct DeferredVirtioIrq {
    irq: IrqNumber,
}

impl DeferredVirtioIrq {
    pub(crate) fn install(self, dev_id: Arc<DeviceId>) -> Result<(), system_error::SystemError> {
        irq_manager().request_irq(
            self.irq,
            "Virtio_IRQ".to_string(),
            &DefaultVirtioIrqHandler,
            IrqHandleFlags::IRQF_SHARED,
            Some(dev_id),
        )
    }
}

pub(crate) enum VirtioIrqSetup {
    Installed(PciVirtioIrqLease),
    Deferred(DeferredVirtioIrq),
}

impl VirtioIrqSetup {
    pub(crate) fn into_parts(self) -> (Option<PciVirtioIrqLease>, Option<DeferredVirtioIrq>) {
        match self {
            Self::Installed(lease) => (Some(lease), None),
            Self::Deferred(irq) => (None, Some(irq)),
        }
    }

    pub(crate) fn into_deferred(self) -> Option<DeferredVirtioIrq> {
        match self {
            Self::Installed(mut lease) => {
                // Legacy callers do not yet support runtime hot-unplug. They
                // still gain automatic rollback until construction commits.
                lease.armed = false;
                None
            }
            Self::Deferred(irq) => Some(irq),
        }
    }
}

/// Owns a published PCI IRQ from reservation through device lifetime.
/// Dropping an uncommitted lease reverses setup and makes the vector reusable.
pub(crate) struct PciVirtioIrqLease {
    device: Arc<PciDeviceStructureGeneralDevice>,
    dev_id: Arc<DeviceId>,
    irq: IrqNumber,
    installed: bool,
    armed: bool,
}

impl PciVirtioIrqLease {
    fn reserved(
        device: Arc<PciDeviceStructureGeneralDevice>,
        dev_id: Arc<DeviceId>,
        irq: IrqNumber,
    ) -> Self {
        Self {
            device,
            dev_id,
            irq,
            installed: false,
            armed: true,
        }
    }

    fn rollback_locked(&mut self) -> Result<(), PciError> {
        if self.installed {
            self.device.irq_uninstall_locked(Some(&self.dev_id))?;
        } else {
            self.device
                .irq_vector
                .write()
                .retain(|irq| *irq != self.irq);
            *self.device.irq_type.write() = IrqType::Unused;
        }
        arch_pci_msi_vector_release(self.irq);
        self.armed = false;
        Ok(())
    }
}

impl Drop for PciVirtioIrqLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.installed {
            if let Err(error) = self.device.irq_uninstall(Some(&self.dev_id)) {
                log::error!("failed to roll back VirtIO PCI IRQ: {error:?}");
                return;
            }
        } else {
            self.device
                .irq_vector
                .write()
                .retain(|irq| *irq != self.irq);
            *self.device.irq_type.write() = IrqType::Unused;
        }
        arch_pci_msi_vector_release(self.irq);
    }
}

/// A validated VirtIO shared-memory region descriptor.
///
/// This is address metadata only. It does not grant access to the region or describe a safe
/// cache policy for normal memory accesses.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VirtioSharedMemoryRegion {
    physical_address: PhysAddr,
    length: u64,
    bar: u8,
    offset: u64,
}

impl VirtioSharedMemoryRegion {
    pub(crate) fn new(physical_address: PhysAddr, length: u64, bar: u8, offset: u64) -> Self {
        Self {
            physical_address,
            length,
            bar,
            offset,
        }
    }

    #[allow(dead_code)]
    pub fn physical_address(&self) -> PhysAddr {
        self.physical_address
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    pub(crate) fn bar(&self) -> u8 {
        self.bar
    }

    pub(crate) fn offset(&self) -> u64 {
        self.offset
    }
}

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

    pub fn irq_is_msix(&self) -> bool {
        match self {
            VirtIOTransport::Pci(transport) => {
                matches!(
                    *transport.pci_device().irq_type.read(),
                    IrqType::Msix { .. }
                )
            }
            VirtIOTransport::Mmio(_) => false,
        }
    }

    pub fn shared_memory_region(&self, id: u8) -> Option<VirtioSharedMemoryRegion> {
        match self {
            VirtIOTransport::Pci(transport) => transport.shared_memory_region(id),
            VirtIOTransport::Mmio(_) => None,
        }
    }

    pub fn reserve_shared_memory_region(
        &self,
        region: VirtioSharedMemoryRegion,
    ) -> Result<PciBarSubresourceGuard, PciError> {
        match self {
            VirtIOTransport::Pci(transport) => transport.reserve_shared_memory_region(region),
            VirtIOTransport::Mmio(_) => Err(PciError::InvalidBarType),
        }
    }

    /// 设置中断
    pub(crate) fn setup_irq(
        &self,
        dev_id: Arc<DeviceId>,
    ) -> Result<VirtioIrqSetup, system_error::SystemError> {
        match self {
            VirtIOTransport::Pci(transport) => {
                let standard_device = transport.pci_device().as_standard_device().unwrap();
                let lifecycle_guard = standard_device.common_header.irq_lifecycle.lock();
                standard_device
                    .ensure_irq_unowned()
                    .map_err(|_| system_error::SystemError::EBUSY)?;
                standard_device
                    .irq_init(IRQ::PCI_IRQ_MSIX | IRQ::PCI_IRQ_MSI)
                    .ok_or(system_error::SystemError::ENODEV)?;
                let irq = match transport.setup_irq_vector() {
                    Ok(irq) => irq,
                    Err(_) => {
                        *standard_device.irq_type.write() = IrqType::Unused;
                        return Err(system_error::SystemError::ENOSPC);
                    }
                };
                let mut lease =
                    PciVirtioIrqLease::reserved(standard_device.clone(), dev_id.clone(), irq);

                let msg = PciIrqMsg {
                    irq_common_message: IrqCommonMsg::init_from(
                        0,
                        "Virtio_IRQ".to_string(),
                        &DefaultVirtioIrqHandler,
                        dev_id,
                    ),
                    irq_specific_message: IrqSpecificMsg::msi_default(),
                };
                standard_device
                    .irq_install(msg)
                    .map_err(|_| system_error::SystemError::EIO)?;
                lease.installed = true;
                if standard_device.irq_enable(true).is_err() {
                    let rollback = lease.rollback_locked();
                    drop(lifecycle_guard);
                    if rollback.is_err() {
                        drop(lease);
                    }
                    return Err(system_error::SystemError::EIO);
                }
                drop(lifecycle_guard);
                Ok(VirtioIrqSetup::Installed(lease))
            }
            VirtIOTransport::Mmio(_) => Ok(VirtioIrqSetup::Deferred(DeferredVirtioIrq {
                irq: self.irq(),
            })),
        }
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
