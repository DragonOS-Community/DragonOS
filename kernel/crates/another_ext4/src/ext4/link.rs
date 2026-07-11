use super::Ext4;
use crate::ext4_defs::*;
use crate::prelude::*;

impl Ext4 {
    /// Link a child inode to a parent directory.
    pub(super) fn link_inode(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let child_link_count = child.inode.link_count();
        let parent_link_count = parent.inode.link_count();
        if child.inode.is_dir() {
            // Prepare all inode metadata before publishing parent/name.  A
            // failure before the final dir_add_entry cannot leave a namespace
            // entry pointing at an inode that cleanup may release.
            self.dir_add_entry(child, parent, "..")?;
            parent.inode.set_link_count(parent_link_count + 1);
            self.write_inode_with_csum(parent)?;
        }
        child.inode.set_link_count(child_link_count + 1);
        self.write_inode_with_csum(child)?;
        if let Err(error) = self.dir_add_entry(parent, child, name) {
            child.inode.set_link_count(child_link_count);
            let mut rollback_ok = self.write_inode_with_csum(child).is_ok();
            if child.inode.is_dir() {
                parent.inode.set_link_count(parent_link_count);
                rollback_ok &= self.write_inode_with_csum(parent).is_ok();
            }
            if !rollback_ok {
                self.poison(ErrCode::EIO);
            }
            return Err(error);
        }
        Ok(())
    }

    /// Unlink a child inode from a parent directory.
    ///
    /// Returns a one-shot reclaim capability when the final link is removed.
    pub(super) fn unlink_inode(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
    ) -> Result<Option<InodeReclaimHandle>> {
        // Remove entry from parent directory
        self.dir_remove_entry(parent, name)?;

        let child_link_cnt = child.inode.link_count();
        if child.inode.is_dir() {
            parent.inode.set_link_count(parent.inode.link_count() - 1);
            if let Err(error) = self.write_inode_with_csum(parent) {
                self.poison(ErrCode::EIO);
                return Err(error);
            }
        }
        let final_link = (child.inode.is_dir() && child_link_cnt <= 2) || child_link_cnt <= 1;
        child
            .inode
            .set_link_count(if final_link { 0 } else { child_link_cnt - 1 });
        if let Err(error) = self.write_inode_with_csum(child) {
            self.poison(ErrCode::EIO);
            return Err(error);
        }
        Ok(final_link.then(|| InodeReclaimHandle::new(child.id, child.inode.generation())))
    }
}
