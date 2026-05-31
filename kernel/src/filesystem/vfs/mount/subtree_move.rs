//! MountList index maintenance for mount(MS_MOVE) subtree moves.
//!
//! This module centralizes the path prefix arithmetic needed to "move a mount subtree
//! from an old mount point to a new mount point" (encapsulated on [`MountPath`]) and
//! the atomic rebuild logic for the four internal index tables of [`MountList`],
//! decoupled from the main `mount.rs`.

use alloc::{string::String, string::ToString, sync::Arc, vec::Vec};
use core::mem;

use hashbrown::HashMap;

use super::{InodeId, MountFS, MountList, MountPath, MountRecord};

impl MountPath {
    /// When a subtree moves from `old_base` to `new_base`, compute the new value of this path.
    ///
    /// If this path equals `old_base` or is strictly within the `old_base` subtree,
    /// returns its new path after the move; otherwise returns `None`, indicating this
    /// path is not within the moved subtree and should be preserved as-is.
    fn relocate(&self, old_base: &str, new_base: &str) -> Option<MountPath> {
        let path = self.as_str();
        if path == old_base {
            return Some(MountPath::from(new_base.to_string()));
        }
        // Strict subpath: after stripping the old_base prefix, the remainder starts with '/'.
        let suffix = path.strip_prefix(old_base).filter(|s| s.starts_with('/'))?;
        Some(MountPath::from(Self::join_base(new_base, suffix)))
    }

    /// Append `suffix` (which starts with `/`) to the new base path `base`.
    fn join_base(base: &str, suffix: &str) -> String {
        if base == "/" {
            // suffix already starts with '/', use it directly as the new path.
            return suffix.to_string();
        }
        let mut result = base.trim_end_matches('/').to_string();
        result.push_str(suffix);
        result
    }
}

impl MountList {
    /// Atomically update all MountList indices after moving a mount subtree.
    ///
    /// Used for mount(MS_MOVE): the moved subtree root mount `root_mount` is moved from
    /// its old mount point to a new one. Unlike [`rewrite_paths`](MountList::rewrite_paths),
    /// this method also updates the mount point inode of the root mount record, which is
    /// necessary to keep `mountpoints` and `mount_list` consistent:
    ///
    /// - All path prefixes within the subtree are rewritten from `old_base` to `new_base`;
    /// - The root mount's mount point inode is updated to `new_root_ino` (the old inode index is discarded);
    /// - Child mounts' mount point inodes remain unchanged;
    /// - Records outside the subtree are preserved as-is.
    ///
    /// The four tables `mounts`/`ino2mp`/`mfs2ino`/`mfs2mp` are rebuilt within a single write lock,
    /// ensuring consistency.
    pub fn move_subtree(
        &self,
        root_mount: &Arc<MountFS>,
        new_root_ino: InodeId,
        old_base: &str,
        new_base: &str,
    ) {
        let mut inner = self.inner.write();
        let old_mounts = mem::take(&mut inner.mounts);
        let mut new_mounts: HashMap<Arc<MountPath>, Vec<MountRecord>> = HashMap::new();
        let mut new_ino2mp = HashMap::new();
        let mut new_mfs2ino = HashMap::new();
        let mut new_mfs2mp = HashMap::new();

        for (old_path, stack) in old_mounts {
            let new_path = match old_path.relocate(old_base, new_base) {
                Some(p) => Arc::new(p),
                None => old_path.clone(),
            };

            let entry = new_mounts.entry(new_path.clone()).or_insert_with(Vec::new);
            for mut rec in stack {
                // The root mount's mount point inode changes with the move; child mounts keep their original inode.
                if Arc::ptr_eq(&rec.fs, root_mount) {
                    rec.ino = Some(new_root_ino);
                }
                if let Some(ino) = rec.ino {
                    new_ino2mp.insert(ino, new_path.clone());
                    new_mfs2ino.insert(rec.fs.clone(), ino);
                }
                new_mfs2mp.insert(rec.fs.clone(), new_path.clone());
                entry.push(rec);
            }
        }

        inner.mounts = new_mounts;
        inner.ino2mp = new_ino2mp;
        inner.mfs2ino = new_mfs2ino;
        inner.mfs2mp = new_mfs2mp;
    }
}
