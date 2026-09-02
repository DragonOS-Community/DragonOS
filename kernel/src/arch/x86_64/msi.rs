use core::sync::atomic::{AtomicU64, Ordering};

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
    allocated: [AtomicU64; 2],
}

impl PciMsiVectorAllocator {
    const fn new() -> Self {
        Self {
            allocated: [AtomicU64::new(0), AtomicU64::new(0)],
        }
    }

    fn alloc(&self) -> Option<IrqNumber> {
        for slot in 0..Self::SLOT_COUNT {
            let word = slot / 64;
            let mask = 1u64 << (slot % 64);
            if self.allocated[word].fetch_or(mask, Ordering::AcqRel) & mask == 0 {
                let vector = if slot < 64 {
                    PCI_MSI_VECTOR_FIRST + slot as u32
                } else {
                    PCI_MSI_VECTOR_FIRST + slot as u32 + 1
                };
                return Some(IrqNumber::new(vector));
            }
        }
        None
    }

    fn release(&self, vector: IrqNumber) {
        let value = vector.data();
        let slot = if (PCI_MSI_VECTOR_FIRST..PCI_MSI_VECTOR_INT80).contains(&value) {
            value - PCI_MSI_VECTOR_FIRST
        } else if (PCI_MSI_VECTOR_INT80 + 1..=PCI_MSI_VECTOR_LAST).contains(&value) {
            value - PCI_MSI_VECTOR_FIRST - 1
        } else {
            return;
        } as usize;
        let word = slot / 64;
        let mask = 1u64 << (slot % 64);
        self.allocated[word].fetch_and(!mask, Ordering::AcqRel);
    }

    const SLOT_COUNT: usize = (PCI_MSI_VECTOR_LAST - PCI_MSI_VECTOR_FIRST) as usize;
}

static PCI_MSI_VECTOR_ALLOCATOR: PciMsiVectorAllocator = PciMsiVectorAllocator::new();

/// Reserves a CPU vector from the x86 range owned by PCI MSI/MSI-X.
///
pub fn arch_pci_msi_vector_alloc() -> Option<IrqNumber> {
    PCI_MSI_VECTOR_ALLOCATOR.alloc()
}

/// Releases a vector only after the owning IRQ action has been synchronously
/// removed, making reuse safe.
pub fn arch_pci_msi_vector_release(vector: IrqNumber) {
    PCI_MSI_VECTOR_ALLOCATOR.release(vector);
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
    use super::{IrqNumber, PciMsiVectorAllocator, PCI_MSI_VECTOR_LAST};

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

        let released = IrqNumber::new(vectors[17]);
        allocator.release(released);
        assert_eq!(allocator.alloc(), Some(released));
    }
}
