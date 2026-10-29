//! Linux-style explicit task diagnostics through /proc/sysrq-trigger.

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

use crate::{
    driver::base::block::SeekFrom,
    filesystem::{
        procfs::template::{Builder, FileOps, ProcFileBuilder, ProcWriteCallback},
        vfs::{FilePrivateData, IndexNode, InodeMode, PrimaryWriteResult},
    },
    libs::mutex::MutexGuard,
    process::cred::{capable, CAPFlags},
};

#[derive(Debug)]
pub(super) struct SysrqFileOps;

impl SysrqFileOps {
    pub(super) fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o200))
            .parent(parent)
            .build()
            .expect("create sysrq-trigger inode")
    }

    fn trigger(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<PrimaryWriteResult, SystemError> {
        drop(data);
        // A global task snapshot must not be exposed to container-local root,
        // including a less privileged writer receiving an already-open fd.
        if !capable(CAPFlags::CAP_SYS_ADMIN) {
            return Err(SystemError::EPERM);
        }
        // Linux write_sysrq_trigger consumes only the first byte, returns the
        // entire count, and leaves f_pos unchanged. Only task-state output is
        // implemented; other keys never dispatch a diagnostic or action.
        if len != 0 && buf.first() == Some(&b't') {
            crate::debug::task_dump::dump_all_tasks()?;
        }
        Ok(PrimaryWriteResult {
            written_len: len,
            next_position: Some(offset),
        })
    }
}

impl FileOps for SysrqFileOps {
    fn configure_open_file(
        &self,
        _data: &FilePrivateData,
        behavior: &mut crate::filesystem::vfs::OpenFileBehavior,
    ) {
        behavior
            .mode
            .remove(crate::filesystem::vfs::file::FileMode::FMODE_CAN_READ);
    }

    fn supports_read_iter_flags(&self) -> bool {
        false
    }

    fn write_callback(&self) -> Option<ProcWriteCallback<Self>> {
        Some(Self::trigger)
    }

    fn seek(
        &self,
        _position: &mut usize,
        _origin: &SeekFrom,
        _data: MutexGuard<FilePrivateData>,
    ) -> Option<Result<(), SystemError>> {
        // noop_llseek: successful seeks return the unchanged file position.
        Some(Ok(()))
    }
}
