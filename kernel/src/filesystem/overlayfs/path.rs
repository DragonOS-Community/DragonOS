use super::inode::OvlInode;
use crate::filesystem::vfs::IndexNode;
use alloc::string::String;
use alloc::sync::Arc;
use system_error::SystemError;

impl OvlInode {
    pub(super) fn upper_root_inode(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let upper_mnt = self.overlay_fs()?.ovl_upper_mnt();
        let upper_inode = upper_mnt.upper_inode.lock();
        upper_inode.clone().ok_or(SystemError::EROFS)
    }

    pub(super) fn workdir_inode(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        Ok(self.overlay_fs()?.workdir.clone())
    }

    pub(super) fn child_redirect(&self, name: &str) -> String {
        if self.redirect.is_empty() {
            String::from(name)
        } else {
            let mut redirect = self.redirect.clone();
            redirect.push('/');
            redirect.push_str(name);
            redirect
        }
    }

    pub(super) fn parent_redirect(&self) -> Option<&str> {
        if self.redirect.is_empty() {
            return None;
        }

        match self.redirect.rsplit_once('/') {
            Some((parent, _)) => Some(parent),
            None => Some(""),
        }
    }

    pub(super) fn current_realdata_inode(&self) -> Result<(Arc<dyn IndexNode>, bool), SystemError> {
        if let Some(inode) = self.upper_inode.lock().clone() {
            return Ok((inode, true));
        }

        let lower_inode = self.lower_inodes.first().ok_or(SystemError::ENOENT)?;
        Ok((lower_inode.clone(), false))
    }
}
