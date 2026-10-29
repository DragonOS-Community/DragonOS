//! Linux may_expand_vm policy. Mapping ownership and usage stay in UserMappings.
use super::*;

/// Snapshot once for an operation which evaluates several VMA transitions.
pub(super) struct VmExpansionLimits {
    address_space: usize,
    data: usize,
    data_max: usize,
}

impl VmExpansionLimits {
    pub(super) fn current() -> Self {
        if !ProcessManager::initialized() {
            return Self {
                address_space: usize::MAX,
                data: usize::MAX,
                data_max: usize::MAX,
            };
        }
        let pcb = ProcessManager::current_pcb();
        let data = pcb.get_rlimit(RLimitID::Data);
        Self {
            address_space: pcb.get_rlimit(RLimitID::As).rlim_cur as usize,
            data: data.rlim_cur as usize,
            data_max: data.rlim_max as usize,
        }
    }

    pub(super) fn allows(&self, used: usize, data: usize, flags: VmFlags, growth: usize) -> bool {
        let Some(growth) = growth.checked_add(MMArch::PAGE_SIZE - 1) else {
            return false;
        };
        let growth = growth >> MMArch::PAGE_SHIFT;
        let fits = |bytes: usize, limit: usize| {
            (bytes >> MMArch::PAGE_SHIFT)
                .checked_add(growth)
                .is_some_and(|pages| pages <= (limit >> MMArch::PAGE_SHIFT))
        };
        if !fits(used, self.address_space) {
            return false;
        }
        !flags.is_data_mapping() || fits(data, self.data)
            // Linux preserves the soft-zero/hard-limit ABI for DATA mappings.
            || (self.data == 0 && fits(data, self.data_max))
    }
}

impl InnerAddressSpace {
    pub(super) fn check_vm_expansion(
        &self,
        flags: VmFlags,
        growth: usize,
    ) -> Result<(), SystemError> {
        if VmExpansionLimits::current().allows(
            self.vma_usage_bytes(),
            self.mappings.data_usage_bytes(),
            flags,
            growth,
        ) {
            Ok(())
        } else {
            Err(SystemError::ENOMEM)
        }
    }

    pub(super) fn check_mmap_limits(
        &self,
        region: VirtRegion,
        flags: VmFlags,
    ) -> Result<(), SystemError> {
        let limits = VmExpansionLimits::current();
        let allows = |growth| {
            limits.allows(
                self.vma_usage_bytes(),
                self.mappings.data_usage_bytes(),
                flags,
                growth,
            )
        };
        // Linux retries with ALL replaced VMA pages, including non-DATA pages.
        if allows(region.size()) || allows(region.size() - self.covered_vma_bytes(region)) {
            Ok(())
        } else {
            Err(SystemError::ENOMEM)
        }
    }
}
