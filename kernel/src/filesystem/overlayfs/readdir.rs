use super::inode::{DirState, OvlInode};
use crate::filesystem::vfs::{DirectoryEntry, IndexNode, DT_CHR, DT_UNKNOWN};
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

pub(super) fn list(inode: &OvlInode) -> Result<Vec<String>, SystemError> {
    Ok(list_entries(inode)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

pub(super) fn list_entries(inode: &OvlInode) -> Result<Vec<DirectoryEntry>, SystemError> {
    // Resolve dot identities before taking the child directory lock. This
    // preserves the parent -> child lock order used by overlay mutations.
    let dot_ino = inode.metadata()?.inode_id.into() as u64;
    let dotdot_ino = inode.parent()?.metadata()?.inode_id.into() as u64;
    let state = inode.dir_state()?;
    let _guard = state.mutation_lock.lock();
    list_entries_locked(inode, &state, Some((dot_ino, dotdot_ino)), true)
}

pub(super) fn list_locked(inode: &OvlInode, state: &DirState) -> Result<Vec<String>, SystemError> {
    Ok(list_entries_locked(inode, state, None, false)?
        .into_iter()
        .map(|entry| entry.name)
        .collect())
}

fn list_entries_locked(
    inode: &OvlInode,
    state: &DirState,
    dot_inos: Option<(u64, u64)>,
    cache_result: bool,
) -> Result<Vec<DirectoryEntry>, SystemError> {
    loop {
        let version = inode.dir_version()?;
        if let Some(entries) = state.cached_readdir(&version) {
            return Ok((*entries).clone());
        }

        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        let mut needs_overlay_metadata = BTreeSet::new();
        if let Some(upper) = inode.upper_inode.lock().clone() {
            merge_layer(
                &upper,
                &mut entries,
                &mut seen,
                &mut needs_overlay_metadata,
                true,
            )?;
        }
        for lower in &inode.lower_inodes {
            merge_layer(
                lower,
                &mut entries,
                &mut seen,
                &mut needs_overlay_metadata,
                false,
            )?;
        }

        let mut mapped_entries = Vec::with_capacity(entries.len());
        for mut entry in entries {
            if needs_overlay_metadata.contains(&entry.name) {
                let mapped_ino = if entry.name == "." {
                    dot_inos.map(|(dot, _)| dot)
                } else if entry.name == ".." {
                    dot_inos.map(|(_, dotdot)| dotdot)
                } else {
                    let child = match super::lookup::find_locked(inode, &entry.name, state) {
                        Ok(child) => child,
                        Err(SystemError::ENOENT) => continue,
                        Err(err) => return Err(err),
                    };
                    match child.metadata() {
                        Ok(metadata) => Some(metadata.inode_id.into() as u64),
                        Err(SystemError::ENOENT) => continue,
                        Err(err) => return Err(err),
                    }
                };
                if let Some(mapped_ino) = mapped_ino {
                    entry.ino = mapped_ino;
                }
            }
            entry.next_cookie = (mapped_entries.len() + 1) as u64;
            mapped_entries.push(entry);
        }

        let current_version = inode.dir_version()?;
        if current_version != version {
            continue;
        }
        let entries = Arc::new(mapped_entries);
        if cache_result {
            state.cache_readdir(&current_version, entries.clone());
        }
        return Ok((*entries).clone());
    }
}

fn merge_layer(
    layer: &Arc<dyn IndexNode>,
    entries: &mut Vec<DirectoryEntry>,
    seen: &mut BTreeSet<String>,
    needs_overlay_metadata: &mut BTreeSet<String>,
    map_all_visible: bool,
) -> Result<(), SystemError> {
    for entry in layer.list_entries()? {
        if !seen.insert(entry.name.clone()) {
            continue;
        }
        let visible = if is_dot_entry(&entry.name) {
            true
        } else if entry.d_type as u16 == DT_CHR || entry.d_type as u16 == DT_UNKNOWN {
            match layer.find(&entry.name) {
                Ok(found) => !OvlInode::is_whiteout_inode_checked(&found)?,
                Err(SystemError::ENOENT) => false,
                Err(err) => return Err(err),
            }
        } else {
            true
        };
        if visible {
            if map_all_visible
                || is_dot_entry(&entry.name)
                || matches!(entry.d_type as u16, DT_CHR | DT_UNKNOWN)
                || entry.d_type as u16 == crate::filesystem::vfs::DT_DIR
            {
                needs_overlay_metadata.insert(entry.name.clone());
            }
            entries.push(entry);
        }
    }
    Ok(())
}

fn is_dot_entry(name: &str) -> bool {
    name == "." || name == ".."
}
