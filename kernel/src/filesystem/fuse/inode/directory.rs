use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::mem::{replace, size_of, take};

use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::vfs::{FileType, IndexNode},
    mm::MemoryManagementArch,
};

use super::super::protocol::{
    fuse_read_struct, FuseDirent, FuseDirentPlus, FuseEntryOut, FuseOpenOut,
};
use super::{FuseLookupCacheEntry, FuseNode};

impl FuseNode {
    fn readdir_request_size_for_limits(max_read: usize, max_pages: usize) -> u32 {
        let max_pages_bytes = core::cmp::max(1, max_pages).saturating_mul(MMArch::PAGE_SIZE);
        let size = core::cmp::min(
            Self::READDIR_BUFFER_SIZE,
            core::cmp::min(max_read, max_pages_bytes),
        );
        debug_assert!(size <= u32::MAX as usize);
        size as u32
    }

    pub(super) fn readdir_request_size(&self) -> u32 {
        Self::readdir_request_size_for_limits(self.conn().max_read(), self.conn().max_pages())
    }

    pub(super) fn cache_lookup_child(
        &self,
        name: &str,
        child: &Arc<FuseNode>,
        generation: u64,
        valid: u64,
        valid_nsec: u32,
    ) {
        if child.nodeid() == self.nodeid {
            return;
        }
        let deadline_ns = Self::cache_deadline(valid, valid_nsec);
        self.prune_lookup_cache();

        let mut removed = Vec::new();
        {
            let mut cache = self.lookup_cache.lock();
            if deadline_ns == 0 {
                if let Some(entry) = cache.remove(name) {
                    removed.push(entry);
                }
            } else {
                if !cache.contains_key(name) && cache.len() >= Self::LOOKUP_CACHE_MAX_ENTRIES {
                    if let Some(victim) = cache.keys().next().cloned() {
                        if let Some(entry) = cache.remove(&victim) {
                            removed.push(entry);
                        }
                    }
                }
                if let Some(entry) = cache.get_mut(name) {
                    if Arc::ptr_eq(&entry.child, child) {
                        entry.generation = generation;
                        entry.deadline_ns = deadline_ns;
                    } else {
                        let old_entry = replace(
                            entry,
                            FuseLookupCacheEntry {
                                child: child.clone(),
                                generation,
                                deadline_ns,
                            },
                        );
                        removed.push(old_entry);
                    }
                } else {
                    cache.insert(
                        name.to_string(),
                        FuseLookupCacheEntry {
                            child: child.clone(),
                            generation,
                            deadline_ns,
                        },
                    );
                }
            }
        }
        Self::clear_removed_lookup_entries(removed);
    }

    pub(super) fn invalidate_lookup_cache(&self, name: &str) {
        if let Some(entry) = self.remove_lookup_cache_entry(name) {
            Self::clear_removed_lookup_entries(vec![entry]);
        }
    }

    pub(super) fn invalidate_child_name(&self, name: &str) {
        let removed = self.remove_lookup_cache_entry(name);
        if let Some(child) = removed.as_ref().map(|entry| entry.child.clone()) {
            child.clear_dname_if(name);
        }
        if let Some(entry) = removed {
            Self::clear_removed_lookup_entries(vec![entry]);
        }
    }

    pub(crate) fn notify_invalidate_child(
        &self,
        name: &str,
        expected_child: Option<u64>,
    ) -> Result<(), SystemError> {
        let removed = self
            .remove_lookup_cache_entry(name)
            .ok_or(SystemError::ENOENT)?;
        let matches = expected_child.is_none_or(|nodeid| removed.child.nodeid() == nodeid);
        removed.child.clear_dname_if(name);
        Self::clear_removed_lookup_entries(vec![removed]);
        self.invalidate_cached_metadata();
        if matches {
            Ok(())
        } else {
            Err(SystemError::ENOENT)
        }
    }

    pub(crate) fn notify_expire_child(&self, name: &str) -> Result<(), SystemError> {
        let mut cache = self.lookup_cache.lock();
        let entry = cache.get_mut(name).ok_or(SystemError::ENOENT)?;
        entry.deadline_ns = 0;
        self.invalidate_cached_metadata();
        Ok(())
    }

    pub(super) fn lookup_cached_child(&self, name: &str) -> Option<Arc<FuseNode>> {
        self.prune_lookup_cache();
        let cache = self.lookup_cache.lock();
        let entry = cache.get(name).cloned()?;
        Some(entry.child)
    }

    fn remove_lookup_cache_entry(&self, name: &str) -> Option<FuseLookupCacheEntry> {
        self.lookup_cache.lock().remove(name)
    }

    fn lookup_cache_entry_expired_or_stale(
        parent_nodeid: u64,
        name: &str,
        entry: &FuseLookupCacheEntry,
        now: u64,
    ) -> bool {
        (entry.deadline_ns != u64::MAX && now >= entry.deadline_ns)
            || entry.child.check_not_stale().is_err()
            || entry.child.generation() != entry.generation
            || entry.child.parent_fuse_nodeid() != parent_nodeid
            || !entry.child.has_dname(name)
    }

    fn take_lookup_cache_entries(&self) -> Vec<FuseLookupCacheEntry> {
        let mut cache = self.lookup_cache.lock();
        take(&mut *cache).into_values().collect()
    }

    pub(crate) fn clear_lookup_cache_tree(&self) {
        Self::clear_removed_lookup_entries(self.take_lookup_cache_entries());
    }

    fn prune_lookup_cache(&self) {
        let now = Self::now_ns();
        let removed = {
            let mut cache = self.lookup_cache.lock();
            let stale_keys: Vec<String> = cache
                .iter()
                .filter_map(|(name, entry)| {
                    if Self::lookup_cache_entry_expired_or_stale(self.nodeid, name, entry, now) {
                        Some(name.clone())
                    } else {
                        None
                    }
                })
                .collect();
            let mut removed = Vec::new();
            for key in stale_keys {
                if let Some(entry) = cache.remove(&key) {
                    removed.push(entry);
                }
            }
            removed
        };
        Self::clear_removed_lookup_entries(removed);
    }

    fn clear_removed_lookup_entries(entries: Vec<FuseLookupCacheEntry>) {
        let mut stack = entries;
        while let Some(entry) = stack.pop() {
            stack.extend(entry.child.take_lookup_cache_entries());
        }
    }

    fn align_dirent_record_len(base_len: usize) -> usize {
        (base_len + Self::FUSE_DIRENT_ALIGN - 1) & !(Self::FUSE_DIRENT_ALIGN - 1)
    }

    fn cache_child_from_entry(&self, entry: &FuseEntryOut, name: &str, request_epoch: u64) {
        let mut consumed = false;
        let result = (|| {
            let md = Self::metadata_from_valid_entry(entry, SystemError::EIO, None)?;
            if entry.nodeid == self.nodeid {
                return Err(SystemError::EIO);
            }
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let child = fs.get_or_create_node_with_generation(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
                1,
            )?;
            consumed = true;
            child.set_dname(name);
            child.merge_cached_metadata_from_daemon(
                md,
                entry.attr_valid,
                entry.attr_valid_nsec,
                request_epoch,
            );
            self.cache_lookup_child(
                name,
                &child,
                entry.generation,
                entry.entry_valid,
                entry.entry_valid_nsec,
            );
            Ok::<(), SystemError>(())
        })();
        if result.is_err() && entry.nodeid != 0 && !consumed {
            let _ = self.conn.queue_forget(entry.nodeid, 1);
        }
    }

    pub(super) fn parse_readdirplus_payload(
        &self,
        payload: &[u8],
        names: &mut Vec<String>,
        mut last_off: u64,
        request_epoch: u64,
    ) -> Result<u64, SystemError> {
        let mut pos: usize = 0;
        while pos + size_of::<FuseDirentPlus>() <= payload.len() {
            let plus: FuseDirentPlus = fuse_read_struct(&payload[pos..])?;
            let dirent = plus.dirent;
            let name_start = pos + size_of::<FuseDirentPlus>();
            let name_end = name_start + dirent.namelen as usize;
            if name_end > payload.len() {
                break;
            }

            let name_bytes = &payload[name_start..name_end];
            if let Ok(name) = core::str::from_utf8(name_bytes) {
                if !name.is_empty() && name != "." && name != ".." {
                    names.push(name.to_string());
                    self.cache_child_from_entry(&plus.entry_out, name, request_epoch);
                }
            } else if plus.entry_out.nodeid != 0 {
                let _ = self.conn.queue_forget(plus.entry_out.nodeid, 1);
            }

            last_off = dirent.off;
            let rec_len = Self::align_dirent_record_len(
                size_of::<FuseDirentPlus>() + dirent.namelen as usize,
            );
            if rec_len == 0 {
                break;
            }
            pos = pos.saturating_add(rec_len);
        }
        Ok(last_off)
    }

    pub(super) fn parse_readdir_payload(
        payload: &[u8],
        names: &mut Vec<String>,
        mut last_off: u64,
    ) -> Result<u64, SystemError> {
        let mut pos: usize = 0;
        while pos + size_of::<FuseDirent>() <= payload.len() {
            let dirent: FuseDirent = fuse_read_struct(&payload[pos..])?;
            let name_start = pos + size_of::<FuseDirent>();
            let name_end = name_start + dirent.namelen as usize;
            if name_end > payload.len() {
                break;
            }

            let name_bytes = &payload[name_start..name_end];
            if let Ok(name) = core::str::from_utf8(name_bytes) {
                if !name.is_empty() && name != "." && name != ".." {
                    names.push(name.to_string());
                }
            }

            last_off = dirent.off;
            let rec_len =
                Self::align_dirent_record_len(size_of::<FuseDirent>() + dirent.namelen as usize);
            if rec_len == 0 {
                break;
            }
            pos = pos.saturating_add(rec_len);
        }
        Ok(last_off)
    }

    pub(super) fn ensure_dir(&self) -> Result<(), SystemError> {
        let md = self.cached_or_fetch_metadata()?;
        if md.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        Ok(())
    }

    pub(super) fn parse_create_reply(
        payload: &[u8],
    ) -> Result<(FuseEntryOut, FuseOpenOut), SystemError> {
        let entry_size = size_of::<FuseEntryOut>();
        let open_size = size_of::<FuseOpenOut>();
        if payload.len() < entry_size + open_size {
            return Err(SystemError::EINVAL);
        }
        let entry: FuseEntryOut = fuse_read_struct(&payload[..entry_size])?;
        let open_out: FuseOpenOut = fuse_read_struct(&payload[entry_size..entry_size + open_size])?;
        Ok((entry, open_out))
    }

    pub(super) fn create_node_from_entry(
        &self,
        entry: &FuseEntryOut,
        name: Option<&str>,
        expected_type: FileType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut consumed = false;
        let result = (|| {
            self.check_not_stale()?;
            let md = Self::metadata_from_valid_entry(entry, SystemError::EIO, Some(expected_type))?;
            if entry.nodeid == self.nodeid {
                return Err(SystemError::EIO);
            }
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let child = fs.get_or_create_node_with_generation(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
                1,
            )?;
            if let Some(name) = name {
                child.set_dname(name);
                self.cache_lookup_child(
                    name,
                    &child,
                    entry.generation,
                    entry.entry_valid,
                    entry.entry_valid_nsec,
                );
            }
            consumed = true;
            child.merge_cached_metadata_from_daemon(
                md,
                entry.attr_valid,
                entry.attr_valid_nsec,
                self.conn.sample_attr_epoch(),
            );
            Ok(child as Arc<dyn IndexNode>)
        })();
        if result.is_err() && entry.nodeid != 0 && !consumed {
            let _ = self.conn.queue_forget(entry.nodeid, 1);
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::FuseNode;
    use crate::{arch::MMArch, mm::MemoryManagementArch};

    #[test]
    fn readdir_request_size_honors_read_and_page_limits() {
        assert_eq!(
            FuseNode::readdir_request_size_for_limits(256 * 1024, 124),
            64 * 1024
        );
        assert_eq!(
            FuseNode::readdir_request_size_for_limits(256 * 1024, 4),
            (4 * MMArch::PAGE_SIZE) as u32
        );
        assert_eq!(
            FuseNode::readdir_request_size_for_limits(8 * 1024, 124),
            8 * 1024
        );
        assert_eq!(
            FuseNode::readdir_request_size_for_limits(4 * 1024, 0),
            MMArch::PAGE_SIZE as u32
        );
    }
}
