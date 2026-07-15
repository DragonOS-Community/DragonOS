use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use core::mem::size_of;

use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::vfs::{DirectoryEntry, FileType, IndexNode},
    mm::MemoryManagementArch,
};

use super::super::protocol::{
    fuse_read_struct, FuseDirent, FuseDirentPlus, FuseEntryOut, FuseOpenOut,
};
use super::{FuseLookupCacheEntry, FuseNode};

impl FuseNode {
    const FUSE_NAME_MAX: usize = 1024;

    pub(super) fn use_readdirplus(&self, offset: u64) -> bool {
        if !self.conn().use_readdirplus() {
            return false;
        }
        if !self.conn().readdirplus_auto() {
            return true;
        }
        if self
            .readdirplus_advised
            .swap(false, core::sync::atomic::Ordering::AcqRel)
        {
            return true;
        }
        offset == 0
    }

    pub(super) fn advise_readdirplus(&self) {
        if self.conn().readdirplus_auto() {
            self.readdirplus_advised
                .store(true, core::sync::atomic::Ordering::Release);
        }
    }

    fn mark_readdirplus_init(&self) {
        if self.conn().readdirplus_auto() {
            self.readdirplus_init
                .store(true, core::sync::atomic::Ordering::Release);
        }
    }

    fn consume_readdirplus_init(&self) -> bool {
        self.readdirplus_init
            .swap(false, core::sync::atomic::Ordering::AcqRel)
    }

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
        if child.dax_dontcache() {
            self.invalidate_lookup_cache(name);
            return;
        }
        let deadline_ticks = Self::cache_deadline(valid, valid_nsec);

        let mut removed = Vec::new();
        {
            let mut cache = self.lookup_cache.lock();
            if deadline_ticks == 0 || child.dax_dontcache() {
                if let Some(entry) = cache.pop(name) {
                    removed.push(entry);
                }
            } else if cache
                .peek(name)
                .is_some_and(|entry| Arc::ptr_eq(&entry.child, child))
            {
                let entry = cache
                    .get_mut(name)
                    .expect("peeked lookup cache entry still exists");
                entry.generation = generation;
                entry.deadline_ticks = deadline_ticks;
            } else if let Some((_key, entry)) = cache.push(
                name.to_string(),
                FuseLookupCacheEntry {
                    child: child.clone(),
                    generation,
                    deadline_ticks,
                },
            ) {
                if !Arc::ptr_eq(&entry.child, child) {
                    removed.push(entry);
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
        let entry = cache.peek_mut(name).ok_or(SystemError::ENOENT)?;
        entry.deadline_ticks = 0;
        self.invalidate_cached_metadata();
        Ok(())
    }

    pub(super) fn lookup_cached_child(&self, name: &str) -> Option<Arc<FuseNode>> {
        let entry = self.lookup_cache.lock().get(name).cloned()?;
        let now = Self::now_ticks();
        if !Self::lookup_cache_entry_expired_or_stale(&entry, now) {
            if entry.child.consume_readdirplus_init() {
                self.advise_readdirplus();
            }
            return Some(entry.child);
        }

        let removed = {
            let mut cache = self.lookup_cache.lock();
            let same_entry = cache.peek(name).is_some_and(|current| {
                Arc::ptr_eq(&current.child, &entry.child)
                    && current.generation == entry.generation
                    && current.deadline_ticks == entry.deadline_ticks
            });
            same_entry.then(|| cache.pop(name)).flatten()
        };
        if let Some(removed) = removed {
            Self::clear_removed_lookup_entries(vec![removed]);
        }
        None
    }

    pub(crate) fn purge_lookup_alias(&self, child: &Arc<FuseNode>) {
        let removed = {
            let mut cache = self.lookup_cache.lock();
            let aliases: Vec<String> = cache
                .iter()
                .filter(|(_, entry)| Arc::ptr_eq(&entry.child, child))
                .map(|(name, _)| name.clone())
                .collect();
            aliases
                .into_iter()
                .filter_map(|name| cache.pop(&name))
                .collect()
        };
        Self::clear_removed_lookup_entries(removed);
    }

    fn remove_lookup_cache_entry(&self, name: &str) -> Option<FuseLookupCacheEntry> {
        self.lookup_cache.lock().pop(name)
    }

    fn lookup_cache_entry_expired_or_stale(entry: &FuseLookupCacheEntry, now: u64) -> bool {
        (entry.deadline_ticks != u64::MAX && now >= entry.deadline_ticks)
            || entry.child.dax_dontcache()
            || entry.child.check_not_stale().is_err()
            || entry.child.generation() != entry.generation
    }

    fn take_lookup_cache_entries(&self) -> Vec<FuseLookupCacheEntry> {
        let mut cache = self.lookup_cache.lock();
        let mut entries = Vec::with_capacity(cache.len());
        while let Some((_name, entry)) = cache.pop_lru() {
            entries.push(entry);
        }
        entries
    }

    pub(crate) fn clear_lookup_cache_tree(&self) {
        Self::clear_removed_lookup_entries(self.take_lookup_cache_entries());
    }

    fn clear_removed_lookup_entries(entries: Vec<FuseLookupCacheEntry>) {
        let mut stack = entries;
        while let Some(entry) = stack.pop() {
            stack.extend(entry.child.take_lookup_cache_entries());
        }
    }

    #[cfg(test)]
    fn align_dirent_record_len(base_len: usize) -> usize {
        (base_len + Self::FUSE_DIRENT_ALIGN - 1) & !(Self::FUSE_DIRENT_ALIGN - 1)
    }

    fn checked_dirent_record_len(header_len: usize, name_len: usize) -> Option<usize> {
        header_len
            .checked_add(name_len)?
            .checked_add(Self::FUSE_DIRENT_ALIGN - 1)
            .map(|len| len & !(Self::FUSE_DIRENT_ALIGN - 1))
    }

    fn forget_readdirplus_from(&self, payload: &[u8], mut pos: usize) {
        while pos
            .checked_add(size_of::<FuseDirentPlus>())
            .is_some_and(|end| end <= payload.len())
        {
            let Ok(plus) = fuse_read_struct::<FuseDirentPlus>(&payload[pos..]) else {
                break;
            };
            let Some(rec_len) = Self::checked_dirent_record_len(
                size_of::<FuseDirentPlus>(),
                plus.dirent.namelen as usize,
            ) else {
                break;
            };
            if rec_len > payload.len() - pos {
                break;
            }
            let name_start = pos + size_of::<FuseDirentPlus>();
            let name_end = name_start + plus.dirent.namelen as usize;
            let name = &payload[name_start..name_end];
            let is_dot = name == b"." || name == b"..";
            if !is_dot && plus.entry_out.nodeid != 0 {
                let _ = self.conn.queue_forget(plus.entry_out.nodeid, 1);
            }
            pos += rec_len;
        }
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
                entry.attr.flags,
            )?;
            consumed = true;
            child.set_dname(name);
            child.set_lookup_attr_flags(entry.attr.flags);
            child.mark_readdirplus_init();
            child.merge_cached_metadata_from_daemon(
                md,
                entry.attr_valid,
                entry.attr_valid_nsec,
                request_epoch,
                entry.attr.flags,
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
        entries: &mut Vec<DirectoryEntry>,
        mut last_off: u64,
        request_epoch: u64,
    ) -> Result<u64, SystemError> {
        let mut pos: usize = 0;
        while pos + size_of::<FuseDirentPlus>() <= payload.len() {
            let plus: FuseDirentPlus = fuse_read_struct(&payload[pos..])?;
            let dirent = plus.dirent;
            let name_start = pos + size_of::<FuseDirentPlus>();
            let Some(rec_len) = Self::checked_dirent_record_len(
                size_of::<FuseDirentPlus>(),
                dirent.namelen as usize,
            ) else {
                self.forget_readdirplus_from(payload, pos);
                break;
            };
            if rec_len > payload.len() - pos {
                break;
            }
            let name_end = name_start + dirent.namelen as usize;

            let name_bytes = &payload[name_start..name_end];
            if name_bytes.is_empty()
                || name_bytes.len() > Self::FUSE_NAME_MAX
                || name_bytes.contains(&b'/')
            {
                self.forget_readdirplus_from(payload, pos);
                return Err(SystemError::EIO);
            }
            let name = match core::str::from_utf8(name_bytes) {
                Ok(name) => name,
                Err(_) => {
                    self.forget_readdirplus_from(payload, pos);
                    return Err(SystemError::EIO);
                }
            };
            entries.push(DirectoryEntry {
                name: name.to_string(),
                ino: dirent.ino,
                d_type: dirent.typ as u8,
                next_cookie: dirent.off,
            });
            if name != "." && name != ".." {
                self.cache_child_from_entry(&plus.entry_out, name, request_epoch);
            }

            last_off = dirent.off;
            pos += rec_len;
        }
        Ok(last_off)
    }

    pub(super) fn parse_readdir_payload(
        payload: &[u8],
        entries: &mut Vec<DirectoryEntry>,
        mut last_off: u64,
    ) -> Result<u64, SystemError> {
        let mut pos: usize = 0;
        while pos + size_of::<FuseDirent>() <= payload.len() {
            let dirent: FuseDirent = fuse_read_struct(&payload[pos..])?;
            let name_start = pos + size_of::<FuseDirent>();
            let Some(rec_len) =
                Self::checked_dirent_record_len(size_of::<FuseDirent>(), dirent.namelen as usize)
            else {
                break;
            };
            if rec_len > payload.len() - pos {
                break;
            }
            let name_end = name_start + dirent.namelen as usize;

            let name_bytes = &payload[name_start..name_end];
            if name_bytes.is_empty()
                || name_bytes.len() > Self::FUSE_NAME_MAX
                || name_bytes.contains(&b'/')
            {
                return Err(SystemError::EIO);
            }
            let name = core::str::from_utf8(name_bytes).map_err(|_| SystemError::EIO)?;
            entries.push(DirectoryEntry {
                name: name.to_string(),
                ino: dirent.ino,
                d_type: dirent.typ as u8,
                next_cookie: dirent.off,
            });

            last_off = dirent.off;
            pos += rec_len;
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
                entry.attr.flags,
            )?;
            child.set_lookup_attr_flags(entry.attr.flags);
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
                entry.attr.flags,
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
    use alloc::vec::Vec;

    use super::FuseNode;
    use crate::{
        arch::MMArch,
        filesystem::{
            fuse::protocol::{fuse_pack_struct, FuseDirent},
            vfs::{DT_DIR, DT_REG},
        },
        mm::MemoryManagementArch,
    };

    fn push_dirent(payload: &mut Vec<u8>, ino: u64, cookie: u64, typ: u32, name: &str) {
        let dirent = FuseDirent {
            ino,
            off: cookie,
            namelen: name.len() as u32,
            typ,
        };
        payload.extend_from_slice(fuse_pack_struct(&dirent));
        payload.extend_from_slice(name.as_bytes());
        payload.resize(FuseNode::align_dirent_record_len(payload.len()), 0);
    }

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

    #[test]
    fn readdir_parser_preserves_type_dot_entries_and_opaque_cookies() {
        let mut payload = Vec::new();
        push_dirent(&mut payload, 10, 7, DT_DIR as u32, ".");
        push_dirent(&mut payload, 11, 41, DT_REG as u32, "file");

        let mut entries = Vec::new();
        let last = FuseNode::parse_readdir_payload(&payload, &mut entries, 0).unwrap();
        assert_eq!(last, 41);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].name, ".");
        assert_eq!(entries[0].next_cookie, 7);
        assert_eq!(entries[1].ino, 11);
        assert_eq!(entries[1].d_type, DT_REG as u8);
        assert_eq!(entries[1].next_cookie, 41);
    }

    #[test]
    fn readdir_parser_rejects_names_with_slashes() {
        let mut payload = Vec::new();
        push_dirent(&mut payload, 10, 1, DT_REG as u32, "bad/name");
        let mut entries = Vec::new();
        assert_eq!(
            FuseNode::parse_readdir_payload(&payload, &mut entries, 0),
            Err(system_error::SystemError::EIO)
        );
    }

    #[test]
    fn readdir_parser_does_not_emit_record_with_truncated_padding() {
        let mut payload = Vec::new();
        push_dirent(&mut payload, 10, 7, DT_REG as u32, "x");
        payload.pop();
        let mut entries = Vec::new();
        assert_eq!(
            FuseNode::parse_readdir_payload(&payload, &mut entries, 0),
            Ok(0)
        );
        assert!(entries.is_empty());
    }
}
