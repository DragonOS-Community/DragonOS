use super::inode::OvlInode;
use crate::filesystem::vfs::{FileType, IndexNode};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

static OVL_TEMP_ID: AtomicUsize = AtomicUsize::new(0);
type WorkdirTemp = (Arc<dyn IndexNode>, Arc<dyn IndexNode>, String);

impl OvlInode {
    pub(super) fn create_workdir_temp<F>(&self, create: F) -> Result<WorkdirTemp, SystemError>
    where
        F: Fn(&Arc<dyn IndexNode>, &str) -> Result<Arc<dyn IndexNode>, SystemError>,
    {
        let workdir = self.workdir_inode()?;
        for _ in 0..32 {
            let id = OVL_TEMP_ID.fetch_add(1, Ordering::Relaxed);
            let name = format!(".dragonos-ovl-{}", id);
            match create(&workdir, &name) {
                Ok(inode) => return Ok((workdir, inode, name)),
                Err(SystemError::EEXIST) => continue,
                Err(err) => return Err(err),
            }
        }

        Err(SystemError::EEXIST)
    }

    pub(super) fn cleanup_workdir_temp(
        workdir: &Arc<dyn IndexNode>,
        name: &str,
    ) -> Result<(), SystemError> {
        let inode = match workdir.find(name) {
            Ok(inode) => inode,
            Err(SystemError::ENOENT) => return Ok(()),
            Err(err) => return Err(err),
        };
        let metadata = inode.metadata()?;

        if metadata.file_type == FileType::Dir {
            workdir.rmdir(name)
        } else {
            workdir.unlink(name)
        }
    }
}
