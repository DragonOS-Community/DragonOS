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
        // Add entry to parent directory
        self.dir_add_entry(parent, child, name)?;

        let child_link_count = child.inode.link_count();
        if child.inode.is_dir() {
            // Link child/".."
            self.dir_add_entry(child, parent, "..")?;
            parent.inode.set_link_count(parent.inode.link_count() + 1);
            self.write_inode_with_csum(parent);
        }
        // Link parent/child
        child.inode.set_link_count(child_link_count + 1);
        self.write_inode_with_csum(child);
        Ok(())
    }

    /// Unlink a child inode from a parent directory.
    ///
    /// If `free` is true, the inode will be freed if it has no links.
    pub(super) fn unlink_inode(
        &self,
        parent: &mut InodeRef,
        child: &mut InodeRef,
        name: &str,
        free: bool,
    ) -> Result<()> {
        // Remove entry from parent directory
        self.dir_remove_entry(parent, name)?;

        let child_link_cnt = child.inode.link_count();
        if child.inode.is_dir() {
            // Child is a directory
            // Unlink "child/.."
            self.dir_remove_entry(child, "..")?;
            parent.inode.set_link_count(parent.inode.link_count() - 1);
            self.write_inode_with_csum(parent);
        }
        if free && ((child.inode.is_dir() && child_link_cnt <= 2) || child_link_cnt <= 1) {
            // Remove file or directory
            return self.free_inode(child);
        }
        child.inode.set_link_count(child_link_cnt - 1);
        self.write_inode_with_csum(child);
        Ok(())
    }
}
