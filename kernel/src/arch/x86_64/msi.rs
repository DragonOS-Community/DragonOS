use core::sync::atomic::{AtomicU32, Ordering};

use system_error::SystemError;

use crate::{
    arch::driver::apic::lapic_vector::local_apic_chip,
    driver::pci::pci_irq::TriggerMode,
    exception::{
        handle::edge_irq_handler, irqdata::IrqLineStatus, irqdesc::irq_desc_manager, IrqNumber,
    },
};

const PCI_MSI_VECTOR_FIRST: u32 = 64;
const PCI_MSI_VECTOR_INT80: u32 = 128;
const PCI_MSI_VECTOR_LAST: u32 = 150;

struct PciMsiVectorAllocator {
    next: AtomicU32,
}

impl PciMsiVectorAllocator {
    const fn new() -> Self {
        Self {
            next: AtomicU32::new(PCI_MSI_VECTOR_FIRST),
        }
    }

    fn alloc(&self) -> Option<IrqNumber> {
        loop {
            let current = self.next.load(Ordering::Relaxed);
            if current > PCI_MSI_VECTOR_LAST {
                return None;
            }
            let next = if current == PCI_MSI_VECTOR_INT80 - 1 {
                PCI_MSI_VECTOR_INT80 + 1
            } else {
                current + 1
            };
            if self
                .next
                .compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return Some(IrqNumber::new(current));
            }
        }
    }
}

static PCI_MSI_VECTOR_ALLOCATOR: PciMsiVectorAllocator = PciMsiVectorAllocator::new();

/// Reserves a CPU vector from the x86 range owned by PCI MSI/MSI-X.
///
/// Vectors are intentionally not reused. DragonOS does not yet have a safe PCI `free_irq`
/// lifecycle, so reuse could route a live MSI action to a different device.
pub fn arch_pci_msi_vector_alloc() -> Option<IrqNumber> {
    PCI_MSI_VECTOR_ALLOCATOR.alloc()
}

/// Rebinds an allocated vector from the blanket IOAPIC setup to message-interrupt semantics.
pub fn arch_pci_msi_vector_setup(vector: IrqNumber) -> Result<(), SystemError> {
    if !(PCI_MSI_VECTOR_FIRST..=PCI_MSI_VECTOR_LAST).contains(&vector.data())
        || vector.data() == PCI_MSI_VECTOR_INT80
    {
        return Err(SystemError::EINVAL);
    }
    let desc = irq_desc_manager()
        .lookup(vector)
        .ok_or(SystemError::EINVAL)?;
    let irq_data = desc.irq_data();
    let mut chip_info = irq_data.chip_info_write_irqsave();
    chip_info.set_chip(Some(local_apic_chip().clone()));
    chip_info.set_chip_data(None);
    drop(chip_info);
    desc.modify_status(IrqLineStatus::IRQ_LEVEL, IrqLineStatus::empty());
    desc.set_handler(edge_irq_handler());
    Ok(())
}
/// @brief 获得MSI Message Address
/// @param processor 目标CPU ID号
/// @return MSI Message Address
pub fn arch_msi_message_address(processor: u16) -> u32 {
    0xfee00000 | ((processor as u32) << 12)
}
/// @brief 获得MSI Message Data
/// @param vector 分配的中断向量号
/// @param processor 目标CPU ID号
/// @param trigger  申请中断的触发模式，MSI默认为边沿触发
/// @return MSI Message Address
pub fn arch_msi_message_data(vector: u16, _processor: u16, trigger: TriggerMode) -> u32 {
    match trigger {
        TriggerMode::EdgeTrigger => vector as u32,
        TriggerMode::AssertHigh => vector as u32 | 1 << 15 | 1 << 14,
        TriggerMode::AssertLow => vector as u32 | 1 << 15,
    }
}

#[cfg(test)]
mod tests {
    use super::{PciMsiVectorAllocator, PCI_MSI_VECTOR_LAST};

    #[test]
    fn pci_msi_vector_allocator_skips_int80_and_exhausts() {
        let allocator = PciMsiVectorAllocator::new();
        let mut vectors = alloc::vec::Vec::new();
        while let Some(vector) = allocator.alloc() {
            vectors.push(vector.data());
        }

        assert_eq!(vectors.first().copied(), Some(64));
        assert!(vectors.contains(&127));
        assert!(!vectors.contains(&128));
        assert!(vectors.contains(&129));
        assert_eq!(vectors.last().copied(), Some(PCI_MSI_VECTOR_LAST));
        assert_eq!(vectors.len(), 86);
        assert_eq!(allocator.alloc(), None);
    }
}
