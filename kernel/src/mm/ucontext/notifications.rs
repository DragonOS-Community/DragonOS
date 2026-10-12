use super::*;

#[derive(Debug)]
pub(crate) struct VmaCloseNotification {
    pub(crate) file: Arc<File>,
    pub(crate) region: VirtRegion,
    pub(crate) vm_flags: VmFlags,
}

#[derive(Default)]
pub(crate) struct VmaCloseNotifications {
    pub(crate) vma: Vec<VmaCloseNotification>,
    pub(crate) sysv: Vec<Arc<SysVShmAttach>>,
    /// Removed originals and unpublished split bodies retire outside MM.
    retired_vmas: Vec<Arc<LockedVMA>>,
}

impl VmaCloseNotifications {
    pub(super) fn is_empty(&self) -> bool {
        self.vma.is_empty() && self.sysv.is_empty() && self.retired_vmas.is_empty()
    }

    pub(super) fn extend(&mut self, mut other: VmaCloseNotifications) {
        Self::append_prepared(&mut self.vma, &mut other.vma);
        Self::append_prepared(&mut self.sysv, &mut other.sysv);
        Self::append_prepared(&mut self.retired_vmas, &mut other.retired_vmas);
    }

    fn append_prepared<T>(destination: &mut Vec<T>, source: &mut Vec<T>) {
        if destination.is_empty() && destination.capacity() < source.len() {
            core::mem::swap(destination, source);
        } else {
            assert!(destination.capacity() - destination.len() >= source.len());
            destination.append(source);
        }
    }

    pub(super) fn prepare_split_retirement(&mut self, count: usize) -> Result<(), SystemError> {
        let additional = count.checked_mul(2).ok_or(SystemError::ENOMEM)?;
        self.prepare_retirement_slots(additional)
    }

    pub(super) fn prepare_retirement_slots(
        &mut self,
        additional: usize,
    ) -> Result<(), SystemError> {
        self.retired_vmas
            .try_reserve_exact(additional)
            .map_err(|_| SystemError::ENOMEM)?;
        self.vma
            .try_reserve_exact(additional)
            .map_err(|_| SystemError::ENOMEM)?;
        self.sysv
            .try_reserve_exact(additional)
            .map_err(|_| SystemError::ENOMEM)
    }

    pub(super) fn retire_vma(&mut self, vma: Arc<LockedVMA>) {
        assert!(self.retired_vmas.len() < self.retired_vmas.capacity());
        self.retired_vmas.push(vma);
    }

    fn retire_split(&mut self, sides: VmaSplitSides) {
        for vma in [sides.0, sides.1].into_iter().flatten() {
            self.retire_vma(vma);
        }
    }
}

pub(super) struct MremapOutcome {
    pub(super) addr: VirtAddr,
    pub(super) notifications: VmaCloseNotifications,
    /// Explicit executable-publication plan for ranges whose uprobe sites must
    /// be installed before their original execute permission is restored.
    pub(super) exec_publication: super::mremap::MremapExecPublication,
    /// Locked pages in this committed range must be populated only after the
    /// caller has released `AddressSpace::write()`.  A file fault can return
    /// `VM_FAULT_RETRY` and wait on PageCache invalidation.
    pub(super) post_commit_population: Option<(VirtRegion, Weak<LockedVMA>)>,
}

/// Result of a committed program-break operation.
///
/// `set_brk()` runs under `AddressSpace::write()` to change the VMA set, but
/// a new heap VMA can inherit `MCL_FUTURE`.  Its population must therefore be
/// deferred to the outer `AddressSpace` owner: a file-backed fault reached by
/// the generic population machinery may return `VM_FAULT_RETRY` and must not
/// wait while that write guard is held.
pub(super) struct BrkOutcome {
    pub(super) old_brk: VirtAddr,
    pub(super) post_commit_population: Option<(VirtRegion, Weak<LockedVMA>)>,
    pub(super) notifications: VmaCloseNotifications,
}

/// A committed locked VMA that must be populated after its owner drops
/// `AddressSpace::write()`.
///
/// This is intentionally an owned request rather than an `InnerAddressSpace`
/// callback: retrying a file-backed fault can block on PageCache invalidation,
/// while the VMA identity must still be revalidated after that wait.
pub(crate) struct LockedPopulationRequest {
    pub(super) region: VirtRegion,
    pub(super) expected_vma: Weak<LockedVMA>,
}

pub(super) struct MremapFailure {
    pub(super) err: MemoryAllocationError,
    pub(super) notifications: VmaCloseNotifications,
    /// A source barrier can survive phase-A rollback. The outer owner must
    /// reapply source sites and publish this plan before dropping mm.write().
    pub(super) exec_publication: super::mremap::MremapExecPublication,
}

impl From<SystemError> for MremapFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err: err.into(),
            notifications: VmaCloseNotifications::default(),
            exec_publication: super::mremap::MremapExecPublication::default(),
        }
    }
}

impl From<MemoryAllocationError> for MremapFailure {
    fn from(err: MemoryAllocationError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
            exec_publication: super::mremap::MremapExecPublication::default(),
        }
    }
}

pub(super) struct MmapFailure {
    pub(super) err: MemoryAllocationError,
    pub(super) notifications: VmaCloseNotifications,
}

impl From<SystemError> for MmapFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err: err.into(),
            notifications: VmaCloseNotifications::default(),
        }
    }
}

impl From<MemoryAllocationError> for MmapFailure {
    fn from(err: MemoryAllocationError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

impl From<VmaOpFailure> for MmapFailure {
    fn from(failure: VmaOpFailure) -> Self {
        Self {
            err: failure.err,
            notifications: failure.notifications,
        }
    }
}

pub(crate) struct VmaOpFailure {
    pub(crate) err: MemoryAllocationError,
    pub(crate) notifications: VmaCloseNotifications,
}

impl From<SystemError> for VmaOpFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err: err.into(),
            notifications: VmaCloseNotifications::default(),
        }
    }
}

impl From<MemoryAllocationError> for VmaOpFailure {
    fn from(err: MemoryAllocationError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

pub(super) struct VmaSplitFailure {
    pub(super) err: MemoryAllocationError,
    pub(super) lifecycle: VmaSplitLifecycle,
}

impl VmaSplitFailure {
    pub(super) fn rollback_into(
        self,
        notifications: &mut VmaCloseNotifications,
    ) -> MemoryAllocationError {
        self.lifecycle.rollback_into(notifications);
        self.err
    }
}

pub(super) struct MunmapVmaPlan {
    pub(super) original_region: VirtRegion,
    pub(super) intersection: VirtRegion,
    pub(super) locked_vm_after_unmap: Option<usize>,
    pub(super) split_lifecycle: VmaSplitLifecycle,
}

pub(super) struct MprotectVmaPlan {
    pub(super) original_region: VirtRegion,
    pub(super) intersection: VirtRegion,
    #[cfg(target_arch = "x86_64")]
    pub(super) old_vm_flags: VmFlags,
    #[cfg(target_arch = "x86_64")]
    pub(super) defer_uprobe_execute: bool,
    pub(super) new_vm_flags: VmFlags,
    pub(super) split_lifecycle: VmaSplitLifecycle,
}

pub(super) struct MadviseVmaPlan {
    pub(super) original_region: VirtRegion,
    pub(super) intersection: VirtRegion,
    pub(super) split_lifecycle: VmaSplitLifecycle,
}

#[derive(Debug)]
pub(super) struct VmaSplitLifecycle {
    pub(super) sysv_shm: Option<Arc<SysVShmAttach>>,
    pub(super) open_count: usize,
    pub(super) file_opens: Vec<VmaCloseNotification>,
    pub(super) committed: bool,
    pub(super) prepared_sides: VmaSplitSides,
    pub(super) split_source: Option<(usize, VirtRegion)>,
}

impl VmaSplitLifecycle {
    pub(super) fn none() -> Self {
        Self {
            sysv_shm: None,
            open_count: 0,
            file_opens: Vec::new(),
            committed: false,
            prepared_sides: (None, None),
            split_source: None,
        }
    }

    pub(super) fn commit(mut self) {
        self.committed = true;
    }

    pub(super) fn rollback_into(mut self, notifications: &mut VmaCloseNotifications) {
        if self.committed {
            return;
        }
        notifications.retire_split(core::mem::take(&mut self.prepared_sides));
        if let Some(sysv_shm) = self.sysv_shm.take() {
            for _ in 0..self.open_count {
                assert!(notifications.sysv.len() < notifications.sysv.capacity());
                notifications.sysv.push(sysv_shm.clone());
            }
        }
        assert!(notifications.vma.capacity() - notifications.vma.len() >= self.file_opens.len());
        notifications.vma.append(&mut self.file_opens);
        self.open_count = 0;
        self.committed = true;
    }

    pub(super) fn failure(self, err: MemoryAllocationError) -> VmaSplitFailure {
        VmaSplitFailure {
            err,
            lifecycle: self,
        }
    }
}

impl Drop for VmaSplitLifecycle {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        error!(
            "VmaSplitLifecycle dropped without explicit commit/rollback; VMA closes must be routed through VmaCloseNotifications"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::VmaCloseNotifications;
    use alloc::vec::Vec;

    #[test]
    fn default_destination_transfers_large_collection() {
        let mut destination = Vec::new();
        let mut source: Vec<usize> = (0..128).collect();
        let allocation = source.as_ptr();
        VmaCloseNotifications::append_prepared(&mut destination, &mut source);
        assert_eq!(destination.as_ptr(), allocation);
        assert_eq!(destination.len(), 128);
        assert!(source.is_empty());
    }

    #[test]
    fn large_fixed_target_tail_and_source_fit_prepared_storage() {
        let (target, tail) = (32usize, 17usize);
        let capacity = 2 * (target + tail) + 4;
        let mut destination = Vec::with_capacity(capacity);
        let allocation = destination.as_ptr();
        for count in [target, 2 * tail, 3] {
            let mut source: Vec<usize> = (0..count).collect();
            VmaCloseNotifications::append_prepared(&mut destination, &mut source);
            assert_eq!(destination.as_ptr(), allocation);
            assert!(source.is_empty());
        }
        assert_eq!(destination.len(), target + 2 * tail + 3);
    }
}
