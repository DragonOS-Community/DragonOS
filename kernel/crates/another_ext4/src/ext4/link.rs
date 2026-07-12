use super::Ext4;
use crate::ext4_defs::*;
use crate::prelude::*;

/// Whether removing one published namespace entry makes the inode an orphan.
/// Directories cannot have hard-link aliases: once rmdir/rename has verified
/// that the directory is empty, Linux clears even an unexpectedly high nlink.
pub(super) fn namespace_removal_is_final(is_dir: bool, link_count: u16) -> bool {
    is_dir || link_count <= 1
}

impl Ext4 {
    /// Link a child inode to a parent directory.
    pub(super) fn link_inode(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
        allow_orphan_relink: bool,
    ) -> Result<()> {
        self.ensure_mutable()?;
        let child_link_count = child.inode.link_count();
        let parent_link_count = parent.inode.link_count();
        if child_link_count == 0 && allow_orphan_relink {
            if !self.legacy_orphan_contains(child.id)? {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            if child.inode.is_dir() {
                return Err(Ext4Error::new(ErrCode::EPERM));
            }
            if !self.dir_has_insert_space(parent, child, name)? {
                self.prepare_empty_dir_slot(parent)?;
            }
            // A zero-link inode is discoverable through the durable orphan
            // chain.  Linux removes it from that chain in the same handle that
            // publishes the new name and link count; otherwise a crash could
            // reclaim a newly reachable inode.
            let mut transaction = self.transaction_start(3)?;
            let mut sb = self.read_super_block_cached();
            self.transaction_orphan_del(&mut transaction, child, &mut sb)?;
            self.transaction_dir_add_existing(&mut transaction, parent, child, name)?;
            child.inode.set_link_count(1);
            child.inode.set_next_orphan(0);
            self.transaction_stage_inode_with_csum(&mut transaction, child)?;
            if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
                self.poison(ErrCode::EIO);
                return Err(error.error);
            }
            return Ok(());
        }
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
        let child_link_cnt = child.inode.link_count();
        // Linux clears an empty directory's link count unconditionally after
        // removing its sole parent entry.  A stale/high directory nlink is a
        // corruption warning, not evidence of another namespace alias.
        let final_link = namespace_removal_is_final(child.inode.is_dir(), child_link_cnt);

        if final_link {
            // Linux journals deletion of the directory entry, the zero link
            // count, and insertion into the orphan list in one handle.  Keep
            // the same crash invariant here: after recovery the inode is
            // either still named, or unreachable and discoverable from the
            // on-disk orphan head.
            let mut transaction =
                self.transaction_start(if child.inode.is_dir() { 4 } else { 3 })?;
            self.transaction_dir_remove_entry(&mut transaction, parent, name)?;

            if child.inode.is_dir() {
                parent.inode.set_link_count(parent.inode.link_count() - 1);
                self.transaction_stage_inode_with_csum(&mut transaction, parent)?;
            }
            child.inode.set_link_count(0);
            let mut sb = self.read_super_block_cached();
            self.transaction_orphan_add(&mut transaction, child, &mut sb)?;

            if let Err(error) = transaction.commit(self.block_device.as_ref(), self) {
                // A commit-path failure may make journal state uncertain.  Do
                // not let legacy direct writers continue after this boundary.
                self.poison(ErrCode::EIO);
                return Err(error.error);
            }
            return Ok(Some(InodeReclaimHandle::new(
                child.id,
                child.inode.generation(),
            )));
        }

        // Non-final hard-link removal does not create an orphan.  Preserve the
        // established path until all namespace writers move under JBD2.
        self.dir_remove_entry(parent, name)?;
        if child.inode.is_dir() {
            parent.inode.set_link_count(parent.inode.link_count() - 1);
            if let Err(error) = self.write_inode_with_csum(parent) {
                self.poison(ErrCode::EIO);
                return Err(error);
            }
        }
        child.inode.set_link_count(child_link_cnt - 1);
        if let Err(error) = self.write_inode_with_csum(child) {
            self.poison(ErrCode::EIO);
            return Err(error);
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::namespace_removal_is_final;

    #[test]
    fn empty_directory_removal_is_final_despite_stale_high_nlink() {
        assert!(namespace_removal_is_final(true, 7));
        assert!(namespace_removal_is_final(true, u16::MAX));
        assert!(namespace_removal_is_final(false, 1));
        assert!(!namespace_removal_is_final(false, 2));
    }
}
