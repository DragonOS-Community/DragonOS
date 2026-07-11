use super::inode::{DirState, OvlInode};
use crate::filesystem::vfs::IndexNode;
use alloc::collections::BTreeSet;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use system_error::SystemError;

pub(super) fn list(inode: &OvlInode) -> Result<Vec<String>, SystemError> {
    let state = inode.dir_state()?;
    let _guard = state.mutation_lock.lock();
    list_locked(inode, &state)
}

pub(super) fn list_locked(inode: &OvlInode, state: &DirState) -> Result<Vec<String>, SystemError> {
    loop {
        let version = inode.dir_version()?;
        if let Some(entries) = state.cached_readdir(&version) {
            return Ok((*entries).clone());
        }

        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        if let Some(upper) = inode.upper_inode.lock().clone() {
            merge_layer(&upper, &mut entries, &mut seen)?;
        }
        for lower in &inode.lower_inodes {
            merge_layer(lower, &mut entries, &mut seen)?;
        }

        let current_version = inode.dir_version()?;
        if current_version != version {
            continue;
        }
        let entries = Arc::new(entries);
        state.cache_readdir(&current_version, entries.clone());
        return Ok((*entries).clone());
    }
}

fn merge_layer(
    layer: &Arc<dyn IndexNode>,
    entries: &mut Vec<String>,
    seen: &mut BTreeSet<String>,
) -> Result<(), SystemError> {
    for name in layer.list()? {
        if !seen.insert(name.clone()) {
            continue;
        }
        if is_dot_entry(&name) {
            entries.push(name);
            continue;
        }
        match layer.find(&name) {
            Ok(found) => {
                if !OvlInode::is_whiteout_inode_checked(&found)? {
                    entries.push(name);
                }
            }
            Err(SystemError::ENOENT) => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

fn is_dot_entry(name: &str) -> bool {
    name == "." || name == ".."
}
