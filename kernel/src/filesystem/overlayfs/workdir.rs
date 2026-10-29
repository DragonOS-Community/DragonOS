use super::inode::OvlInode;
use crate::filesystem::vfs::{mount::DentryMutationContext, FileType, IndexNode};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

static OVL_TEMP_ID: AtomicUsize = AtomicUsize::new(0);
type WorkdirTemp = (Arc<dyn IndexNode>, Arc<dyn IndexNode>, String);

impl OvlInode {
    pub(super) fn create_workdir_temp<F>(
        &self,
        create: F,
    ) -> Result<WorkdirTemp, crate::mm::allocation::MemoryAllocationError>
    where
        F: Fn(
            &Arc<dyn IndexNode>,
            &str,
        ) -> Result<Arc<dyn IndexNode>, crate::mm::allocation::MemoryAllocationError>,
    {
        let workdir = self.workdir_inode()?;
        for _ in 0..32 {
            let id = OVL_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!(".dragonos-ovl-{}", id);
            match create(&workdir, &name) {
                Ok(inode) => return Ok((workdir, inode, name)),
                Err(crate::mm::allocation::MemoryAllocationError::System(SystemError::EEXIST)) => {
                    continue
                }
                Err(err) => return Err(err),
            }
        }

        Err(SystemError::EEXIST.into())
    }

    pub(super) fn cleanup_workdir_temp(
        workdir: &Arc<dyn IndexNode>,
        name: &str,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        Self::cleanup_workdir_temp_with_context(workdir, name, None)
    }

    pub(super) fn cleanup_workdir_temp_with_context(
        workdir: &Arc<dyn IndexNode>,
        name: &str,
        context: Option<&DentryMutationContext<'_>>,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let inode = match super::lookup::lookup_backing(workdir, name) {
            Ok(inode) => inode,
            Err(SystemError::ENOENT) => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        let metadata = inode.metadata()?;

        if metadata.file_type == FileType::Dir {
            match context {
                Some(context) => workdir.rmdir_with_context(name, context),
                None => workdir.rmdir(name),
            }
        } else {
            match context {
                Some(context) => workdir.unlink_with_context(name, context),
                None => workdir.unlink(name),
            }
            .map(|_| ())
        }
    }
}
