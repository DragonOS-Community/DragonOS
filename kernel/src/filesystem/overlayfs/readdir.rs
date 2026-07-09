use super::inode::OvlInode;
use alloc::string::String;
use alloc::vec::Vec;
use system_error::SystemError;

pub(super) fn list(inode: &OvlInode) -> Result<Vec<String>, system_error::SystemError> {
    let mut entries: Vec<String> = Vec::new();
    let mut hidden_entries: Vec<String> = Vec::new();
    let upper_entries = if let Some(ref upper_inode) = *inode.upper_inode.lock() {
        upper_inode.list()?
    } else {
        Vec::new()
    };

    for entry in upper_entries {
        if !inode.has_whiteout(&entry) {
            entries.push(entry);
        }
    }

    for lower_inode in &inode.lower_inodes {
        let lower_entries = lower_inode.list()?;
        for entry in lower_entries {
            if entries.contains(&entry) || hidden_entries.contains(&entry) {
                continue;
            }
            if is_dot_entry(&entry) {
                entries.push(entry);
                continue;
            }
            if inode.has_whiteout(&entry) {
                hidden_entries.push(entry);
                continue;
            }
            match lower_inode.find(&entry) {
                Ok(found) => {
                    if OvlInode::is_whiteout_inode(&found) {
                        hidden_entries.push(entry);
                        continue;
                    }
                }
                Err(SystemError::ENOENT) => continue,
                Err(err) => return Err(err),
            }
            entries.push(entry);
        }
    }

    Ok(entries)
}

fn is_dot_entry(name: &str) -> bool {
    name == "." || name == ".."
}
