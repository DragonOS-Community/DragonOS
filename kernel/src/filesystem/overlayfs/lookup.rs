use super::inode::OvlInode;
use crate::filesystem::vfs::{FileType, IndexNode};
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

impl OvlInode {
    pub(super) fn lower_positive(&self, name: &str) -> bool {
        for lower in &self.lower_inodes {
            match lower.find(name) {
                Ok(found) => {
                    // A metadata failure must not turn a possibly positive lower entry into a
                    // pure-upper decision. Linux makes the same conservative choice for lower
                    // lookup errors in ovl_lower_positive().
                    return match Self::is_whiteout_inode_checked(&found) {
                        Ok(is_whiteout) => !is_whiteout,
                        Err(_) => true,
                    };
                }
                Err(SystemError::ENOENT) | Err(SystemError::ENAMETOOLONG) => continue,
                Err(_) => return true,
            }
        }
        false
    }
}

pub(super) fn find(
    inode: &OvlInode,
    name: &str,
) -> Result<Arc<dyn IndexNode>, system_error::SystemError> {
    let mut upper_inode = None;
    let mut upper_file_type = None;
    if let Some(ref upper) = *inode.upper_inode.lock() {
        match upper.find(name) {
            Ok(found) => {
                if OvlInode::is_whiteout_inode(&found) {
                    return Err(SystemError::ENOENT);
                }
                upper_file_type = Some(found.metadata()?.file_type);
                upper_inode = Some(found);
            }
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }
    }

    if inode.has_whiteout(name) {
        return Err(SystemError::ENOENT);
    }

    let mut lower_inodes = Vec::new();
    if matches!(upper_file_type, None | Some(FileType::Dir)) {
        let mut merge_dirs = upper_file_type == Some(FileType::Dir);
        for lower in &inode.lower_inodes {
            match lower.find(name) {
                Ok(found) => {
                    if OvlInode::is_whiteout_inode(&found) {
                        if upper_inode.is_none() {
                            return Err(SystemError::ENOENT);
                        }
                        break;
                    }
                    let lower_file_type = found.metadata()?.file_type;
                    if merge_dirs {
                        if lower_file_type == FileType::Dir {
                            lower_inodes.push(found);
                            continue;
                        }
                        break;
                    }

                    lower_inodes.push(found);
                    if lower_file_type == FileType::Dir {
                        merge_dirs = true;
                    } else {
                        break;
                    }
                }
                Err(SystemError::ENOENT) => {}
                Err(err) => return Err(err),
            }
        }
    }

    if upper_inode.is_none() && lower_inodes.is_empty() {
        return Err(SystemError::ENOENT);
    }

    let file_type = if let Some(file_type) = upper_file_type {
        file_type
    } else {
        lower_inodes[0].metadata()?.file_type
    };

    let child = Arc::new(OvlInode::new(
        inode.child_redirect(name),
        file_type,
        upper_inode,
        lower_inodes,
    ));
    child.set_fs(inode.fs.lock().clone());

    Ok(child)
}
