use super::*;

pub(crate) struct VmaCloseNotification {
    pub(crate) file: Arc<File>,
    pub(crate) region: VirtRegion,
    pub(crate) vm_flags: VmFlags,
}

#[derive(Default)]
pub(crate) struct VmaCloseNotifications {
    pub(crate) vma: Vec<VmaCloseNotification>,
    pub(crate) sysv: Vec<Arc<SysVShmAttach>>,
}

impl VmaCloseNotifications {
    pub(super) fn is_empty(&self) -> bool {
        self.vma.is_empty() && self.sysv.is_empty()
    }

    pub(super) fn extend(&mut self, mut other: VmaCloseNotifications) {
        self.vma.append(&mut other.vma);
        self.sysv.append(&mut other.sysv);
    }
}

pub(super) struct MremapOutcome {
    pub(super) addr: VirtAddr,
    pub(super) notifications: VmaCloseNotifications,
}

pub(super) struct MremapFailure {
    pub(super) err: SystemError,
    pub(super) notifications: VmaCloseNotifications,
}

impl From<SystemError> for MremapFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

pub(super) struct MmapFailure {
    pub(super) err: SystemError,
    pub(super) notifications: VmaCloseNotifications,
}

impl From<SystemError> for MmapFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

pub(crate) struct VmaOpFailure {
    pub(crate) err: SystemError,
    pub(crate) notifications: VmaCloseNotifications,
}

impl From<SystemError> for VmaOpFailure {
    fn from(err: SystemError) -> Self {
        Self {
            err,
            notifications: VmaCloseNotifications::default(),
        }
    }
}

pub(super) struct VmaSplitFailure {
    pub(super) err: SystemError,
    pub(super) lifecycle: VmaSplitLifecycle,
}

impl VmaSplitFailure {
    pub(super) fn rollback_into(self, notifications: &mut VmaCloseNotifications) -> SystemError {
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
    pub(super) committed: bool,
}

impl VmaSplitLifecycle {
    pub(super) fn none() -> Self {
        Self {
            sysv_shm: None,
            open_count: 0,
            committed: false,
        }
    }

    pub(super) fn commit(mut self) {
        self.committed = true;
    }

    pub(super) fn rollback_into(mut self, notifications: &mut VmaCloseNotifications) {
        if self.committed {
            return;
        }
        if let Some(sysv_shm) = self.sysv_shm.take() {
            for _ in 0..self.open_count {
                notifications.sysv.push(sysv_shm.clone());
            }
        }
        self.open_count = 0;
        self.committed = true;
    }

    pub(super) fn failure(self, err: SystemError) -> VmaSplitFailure {
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
            "VmaSplitLifecycle dropped without explicit commit/rollback; SysV SHM close must be routed through VmaCloseNotifications"
        );
    }
}
