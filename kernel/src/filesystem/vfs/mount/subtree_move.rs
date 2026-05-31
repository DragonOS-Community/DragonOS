//! MountList index maintenance for mount(MS_MOVE) subtree moves.
//!
//! This module centralizes the path prefix arithmetic needed to "move a mount subtree
//! from an old mount point to a new mount point" (encapsulated on [`MountPath`]) and
//! the atomic rebuild logic for the four internal index tables of [`MountList`],
//! decoupled from the main `mount.rs`.

use alloc::{string::String, string::ToString, sync::Arc, vec::Vec};
use core::mem;

use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

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
        moving_mounts: &HashSet<Arc<MountFS>>,
        new_root_ino: InodeId,
        old_base: &str,
        new_base: &str,
    ) -> Result<(), SystemError> {
        let mut inner = self.inner.write();

        for (old_path, stack) in inner.mounts.iter() {
            for rec in stack {
                if moving_mounts.contains(&rec.fs)
                    && old_path.relocate(old_base, new_base).is_none()
                {
                    log::warn!(
                        "move_subtree: moving mount {:?} path '{}' is not under old base '{}'",
                        rec.fs.mount_id(),
                        old_path.as_str(),
                        old_base
                    );
                    return Err(SystemError::EINVAL);
                }
            }
        }

        let old_mounts = mem::take(&mut inner.mounts);
        let mut new_mounts: HashMap<Arc<MountPath>, Vec<MountRecord>> = HashMap::new();
        let mut new_ino2mp = HashMap::new();
        let mut new_mfs2ino = HashMap::new();
        let mut new_mfs2mp = HashMap::new();
        let mut stationary: Vec<(Arc<MountPath>, usize, MountRecord)> = Vec::new();
        let mut moved: Vec<(Arc<MountPath>, usize, MountRecord)> = Vec::new();

        for (old_path, stack) in old_mounts {
            for (idx, rec) in stack.into_iter().enumerate() {
                if moving_mounts.contains(&rec.fs) {
                    moved.push((old_path.clone(), idx, rec));
                } else {
                    stationary.push((old_path.clone(), idx, rec));
                }
            }
        }

        stationary.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(a.1.cmp(&b.1)));
        moved.sort_by(|a, b| a.0.as_str().cmp(b.0.as_str()).then(a.1.cmp(&b.1)));

        let mut ordered_records: Vec<(Arc<MountPath>, MountRecord)> =
            Vec::with_capacity(stationary.len() + moved.len());
        for (path, _, rec) in stationary {
            ordered_records.push((path, rec));
        }
        for (old_path, _, mut rec) in moved {
            let new_path = Arc::new(old_path.relocate(old_base, new_base).ok_or_else(|| {
                log::warn!(
                    "move_subtree: moving mount {:?} path '{}' is not under old base '{}'",
                    rec.fs.mount_id(),
                    old_path.as_str(),
                    old_base
                );
                SystemError::EINVAL
            })?);

            // The root mount's mount point inode changes with the move; child mounts keep their original inode.
            if Arc::ptr_eq(&rec.fs, root_mount) {
                rec.ino = Some(new_root_ino);
            }
            ordered_records.push((new_path, rec));
        }

        for (path, rec) in ordered_records {
            if let Some(ino) = rec.ino {
                new_ino2mp.insert(ino, path.clone());
                new_mfs2ino.insert(rec.fs.clone(), ino);
            }
            new_mfs2mp.insert(rec.fs.clone(), path.clone());
            new_mounts.entry(path).or_insert_with(Vec::new).push(rec);
        }

        inner.mounts = new_mounts;
        inner.ino2mp = new_ino2mp;
        inner.mfs2ino = new_mfs2ino;
        inner.mfs2mp = new_mfs2mp;

        Ok(())
    }
}
