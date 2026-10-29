//! Open-description storage is admitted and physically allocated before open
//! side effects. Body initialization does not allocate or fail; the retained
//! provider open hook can still return its original typed failure.

use super::{
    object_for_task, AllocatedBox, File, FileData, KernelByteKind, MemoryAllocationError, OwnedBox,
    ProcessManager,
};

#[derive(Debug)]
pub(crate) struct PreparedFile {
    storage: AllocatedBox<FileData>,
    epitems: alloc::sync::Arc<crate::filesystem::epoll::event_poll::EPollItemList>,
}

impl PreparedFile {
    /// Guardless storage-only recovery. Call before instance reservations,
    /// inode open side effects and publication; this does not open a File.
    pub(crate) fn new_recovering() -> Result<Self, system_error::SystemError> {
        let mut state = crate::cgroup::memory::runtime::ChargeRecoveryState::default();
        loop {
            match Self::try_new() {
                Ok(storage) => return Ok(storage),
                Err(error) => error.recover(&mut state)?,
            }
        }
    }

    pub(crate) fn try_new() -> Result<Self, MemoryAllocationError> {
        let creator = object_for_task(&ProcessManager::current_pcb());
        Self::try_new_for(&creator)
    }

    pub(crate) fn try_new_for(
        creator: &alloc::sync::Arc<crate::cgroup::memory::object::ObjectAccount>,
    ) -> Result<Self, MemoryAllocationError> {
        let storage = OwnedBox::prepare(creator, KernelByteKind::SlabUnreclaimable)?
            .try_allocate()
            .map_err(|_| {
                crate::mm::allocation::MemoryAllocationError::System(
                    system_error::SystemError::ENOMEM,
                )
            })?;
        // Default initializes only an empty list and lock. Its thin shared
        // header is physically fallible, but is not another charged File body.
        let epitems = alloc::sync::Arc::try_new(
            crate::filesystem::epoll::event_poll::EPollItemList::default(),
        )
        .map_err(|_| system_error::SystemError::ENOMEM)?;
        Ok(Self { storage, epitems })
    }

    pub(super) fn init(
        self,
        make_data: impl FnOnce(
            alloc::sync::Arc<crate::filesystem::epoll::event_poll::EPollItemList>,
        ) -> FileData,
    ) -> File {
        File {
            data: self.storage.init(make_data(self.epitems)),
        }
    }
}
