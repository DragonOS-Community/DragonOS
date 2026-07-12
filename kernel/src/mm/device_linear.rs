use alloc::vec::Vec;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    libs::mutex::Mutex,
    mm::{
        kernel_mapper::KernelMapper,
        page::{CreatedPageTable, EntryFlags},
        tlb::flush_tlb_kernel_range,
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
};

lazy_static! {
    /// Transaction-owned page tables that could not be reclaimed because another device mapping
    /// still used them. Every device-linear teardown retries these records after its synchronous
    /// kernel TLB shootdown.
    static ref DEVICE_MAPPING_TEARDOWN: Mutex<Vec<CreatedPageTable>> = Mutex::new(Vec::new());
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MappingSegment {
    vaddr: VirtAddr,
    paddr: PhysAddr,
    level: usize,
    size: usize,
}

fn supported_levels() -> Vec<usize> {
    #[cfg(target_arch = "x86_64")]
    {
        use raw_cpuid::CpuId;

        let mut levels = Vec::with_capacity(3);
        let has_1g = CpuId::new()
            .get_extended_processor_and_feature_identifiers()
            .is_some_and(|features| features.has_1gib_pages());
        if has_1g && MMArch::PAGE_LEVELS > 2 {
            levels.push(2);
        }
        if MMArch::PAGE_LEVELS > 1 {
            levels.push(1);
        }
        levels.push(0);
        levels
    }

    #[cfg(not(target_arch = "x86_64"))]
    {
        alloc::vec![0]
    }
}

fn level_size(level: usize) -> Option<usize> {
    let shift = level
        .checked_mul(MMArch::PAGE_ENTRY_SHIFT)?
        .checked_add(MMArch::PAGE_SHIFT)?;
    1usize.checked_shl(shift as u32)
}

/// RAII owner for a write-back device-memory mapping in the kernel linear address space.
#[derive(Debug)]
pub struct DeviceLinearMapping {
    start: VirtAddr,
    end: VirtAddr,
    segments: Vec<MappingSegment>,
    created_tables: Vec<CreatedPageTable>,
}

impl DeviceLinearMapping {
    pub fn new(
        paddr: PhysAddr,
        length: usize,
        flags: EntryFlags<MMArch>,
    ) -> Result<Self, SystemError> {
        if length == 0
            || !paddr.check_aligned(MMArch::PAGE_SIZE)
            || !length.is_multiple_of(MMArch::PAGE_SIZE)
        {
            return Err(SystemError::EINVAL);
        }
        let vaddr = unsafe { MMArch::phys_2_virt(paddr) }.ok_or(SystemError::EOVERFLOW)?;
        let end_data = vaddr
            .data()
            .checked_add(length)
            .ok_or(SystemError::EOVERFLOW)?;
        let mut mapping = Self {
            start: vaddr,
            end: VirtAddr::new(end_data),
            segments: Vec::new(),
            created_tables: Vec::new(),
        };

        let levels = supported_levels();
        let mut offset = 0usize;
        let mut kernel_mapper = KernelMapper::lock();
        let mapper = kernel_mapper
            .as_mut()
            .ok_or(SystemError::EAGAIN_OR_EWOULDBLOCK)?;
        if !unsafe { mapper.has_existing_top_level_subtrees(vaddr, mapping.end) } {
            return Err(SystemError::ERANGE);
        }
        while offset < length {
            let current_vaddr = vaddr + offset;
            let current_paddr = paddr + offset;
            let remaining = length - offset;
            let mut installed = None;
            for level in levels.iter().copied() {
                let Some(size) = level_size(level) else {
                    continue;
                };
                if size > remaining
                    || !current_vaddr.check_aligned(size)
                    || !current_paddr.check_aligned(size)
                {
                    continue;
                }
                let result = unsafe {
                    mapper.map_phys_at_level(
                        current_vaddr,
                        current_paddr,
                        level,
                        flags.set_page_global(false),
                        &mut mapping.created_tables,
                    )
                };
                if let Some(flush) = result {
                    unsafe { flush.ignore() };
                    installed = Some(MappingSegment {
                        vaddr: current_vaddr,
                        paddr: current_paddr,
                        level,
                        size,
                    });
                    break;
                }
            }
            let Some(segment) = installed else {
                drop(kernel_mapper);
                mapping.reset();
                return Err(SystemError::EBUSY);
            };
            offset += segment.size;
            mapping.segments.push(segment);
        }
        drop(kernel_mapper);
        Ok(mapping)
    }

    pub fn len(&self) -> usize {
        self.end.data() - self.start.data()
    }

    pub fn reset(&mut self) {
        if self.segments.is_empty() && self.created_tables.is_empty() {
            return;
        }
        // Serialize the complete clear -> shootdown -> reclaim transaction. Otherwise another
        // teardown could observe a deferred table as empty and free it before this mapping's
        // remote TLB entries have been invalidated.
        let mut deferred = DEVICE_MAPPING_TEARDOWN.lock();
        {
            let mut kernel_mapper = KernelMapper::lock();
            let mapper = kernel_mapper
                .as_mut()
                .expect("device mapping reset while KernelMapper is recursively locked");
            for segment in self.segments.iter().rev() {
                let cleared =
                    unsafe { mapper.clear_mapping_at_level(segment.vaddr, segment.level) };
                assert_eq!(
                    cleared.map(|(paddr, _)| paddr),
                    Some(segment.paddr),
                    "device mapping ownership changed before reset"
                );
            }
        }

        // PTE writes are complete and the KernelMapper lock is no longer held. The synchronous
        // shootdown must finish before any transaction-owned page-table page is reclaimed.
        flush_tlb_kernel_range(self.start, self.end, !self.created_tables.is_empty());

        {
            let mut kernel_mapper = KernelMapper::lock();
            let mapper = kernel_mapper
                .as_mut()
                .expect("device page-table reclaim while KernelMapper is recursively locked");
            deferred.append(&mut self.created_tables);
            unsafe { mapper.reclaim_created_tables(&mut deferred) };
        }
        self.segments.clear();
    }
}

impl Drop for DeviceLinearMapping {
    fn drop(&mut self) {
        self.reset();
    }
}

#[cfg(test)]
mod tests {
    use super::level_size;

    #[test]
    fn x86_page_table_levels_have_expected_sizes() {
        assert_eq!(level_size(0), Some(4 * 1024));
        assert_eq!(level_size(1), Some(2 * 1024 * 1024));
        assert_eq!(level_size(2), Some(1024 * 1024 * 1024));
    }
}
