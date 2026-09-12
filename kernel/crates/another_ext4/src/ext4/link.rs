use super::orphan::{final_unlink_orphan_action, FinalUnlinkOrphanAction};
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
    /// Conservative distinct-home budget for namespace directory growth.
    /// At depth d, one append touches at most d old nodes and allocates at
    /// most d+2 new nodes plus a directory block. Each allocation can touch
    /// a separate bitmap/GDT pair; SB and the directory inode add two homes.
    /// 5*d+16 therefore bounds this set even without physical-block sharing.
    /// `other_homes` covers inode creation, orphan changes and fixed dirents.
    pub(super) fn namespace_transaction_credits(
        &self,
        growing_dirs: &[&InodeRef],
        other_homes: usize,
    ) -> Result<usize> {
        growing_dirs.iter().try_fold(other_homes, |credits, dir| {
            let depth = dir.inode.extent_root().header().depth() as usize;
            if depth > 5 {
                return Err(Ext4Error::new(ErrCode::EIO));
            }
            credits
                .checked_add(5 * depth + 16)
                .ok_or_else(|| Ext4Error::new(ErrCode::E2BIG))
        })
    }

    pub(super) fn commit_namespace_transaction(
        &self,
        transaction: super::journal_transaction::Transaction<'_>,
    ) -> Result<()> {
        self.commit_metadata_operation(transaction)
            .map(|_| ())
            .map_err(|failure| {
                if failure.poisoned {
                    self.poison(ErrCode::EIO);
                }
                failure.error
            })
    }

    /// Compose the new name, link counts and optional orphan removal in the
    /// caller's private operation. Errors require aborting that operation.
    pub(super) fn transaction_link_inode(
        &self,
        transaction: &mut super::journal_transaction::Transaction<'_>,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
        allow_orphan_relink: bool,
    ) -> Result<()> {
        let child_links = child.inode.link_count();
        if child_links == 0 && allow_orphan_relink {
            if self.legacy_orphan_membership(child)?
                != super::orphan::LegacyOrphanMembership::ZeroLink
            {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            if child.inode.is_dir() {
                return Err(Ext4Error::new(ErrCode::EPERM));
            }
            let mut sb = self.transaction_read_super_block(transaction)?;
            self.transaction_orphan_del(transaction, child, &mut sb)?;
            child.inode.set_next_orphan(0);
        }
        if child.inode.is_dir() {
            let links = parent
                .inode
                .link_count()
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EMLINK))?;
            self.transaction_dir_add(transaction, child, parent, "..")?;
            parent.inode.set_link_count(links);
        }
        child.inode.set_link_count(
            child_links
                .checked_add(1)
                .ok_or_else(|| Ext4Error::new(ErrCode::EMLINK))?,
        );
        self.transaction_stage_inode_with_csum(transaction, child)?;
        self.transaction_dir_add(transaction, parent, child, name)?;
        // Directory growth already stages its inode. Otherwise only a new
        // subdirectory changes the parent link count in this operation.
        if child.inode.is_dir() {
            self.transaction_stage_inode_with_csum(transaction, parent)?;
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
            let orphan_action = if self.uses_journal() {
                final_unlink_orphan_action(self.legacy_orphan_membership(child)?)?
            } else {
                FinalUnlinkOrphanAction::AddZeroLink
            };
            let mut transaction =
                self.transaction_start(if child.inode.is_dir() { 4 } else { 3 })?;
            self.transaction_dir_remove_entry(&mut transaction, parent, name)?;

            if child.inode.is_dir() {
                parent.inode.set_link_count(parent.inode.link_count() - 1);
                self.transaction_stage_inode_with_csum(&mut transaction, parent)?;
            }
            child.inode.set_link_count(0);
            if self.uses_journal() {
                let mut sb = self.read_super_block_cached();
                match orphan_action {
                    FinalUnlinkOrphanAction::AddZeroLink => {
                        self.transaction_orphan_add_zero_link(&mut transaction, child, &mut sb)?;
                    }
                    FinalUnlinkOrphanAction::PreserveLinkedTail => {
                        // The existing chain position and `next_orphan` are
                        // still the only durable route to this now-zero-link
                        // inode.  Re-adding it could self-loop at the head.
                        self.transaction_stage_inode_with_csum(&mut transaction, child)?;
                    }
                }
            } else {
                // Linux nojournal mode does not enroll newly unlinked inodes in
                // the persistent orphan chain. Lifetime reclaim starts only
                // after this namespace transaction has published nlink == 0.
                child.inode.set_next_orphan(0);
                self.transaction_stage_inode_with_csum(&mut transaction, child)?;
            }

            self.commit_namespace_transaction(transaction)?;
            return Ok(Some(InodeReclaimHandle::new(
                child.id,
                child.inode.generation(),
            )));
        }

        let mut transaction = self.transaction_start(2)?;
        self.transaction_dir_remove_entry(&mut transaction, parent, name)?;
        child.inode.set_link_count(child_link_cnt - 1);
        self.transaction_stage_inode_with_csum(&mut transaction, child)?;
        self.commit_namespace_transaction(transaction)?;
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
