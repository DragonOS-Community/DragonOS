use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{mem::size_of, sync::atomic::Ordering};

use system_error::SystemError;

use crate::{
    arch::MMArch,
    exception::workqueue::{schedule_work, Work},
    filesystem::{
        page_cache::{PageCache, PageCacheBackend, PageCacheReadDmaReservation},
        vfs::{file::FileFlags, FilePrivateData, FileType, IndexNode, Metadata, SetMetadataMask},
    },
    libs::mutex::Mutex,
    mm::{readahead::FileReadaheadState, MemoryManagementArch},
    time::PosixTimeSpec,
};

use super::super::{
    conn::{BackgroundReadPagesCtx, FuseRequestCred},
    private_data::{
        FuseFilePrivateData, FuseOpenContext, FuseOpenLifetime, FuseOpenPrivateData,
        FuseWritebackHandle,
    },
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAttrOut, FuseFlushIn, FuseFsyncIn, FuseOpenIn,
        FuseOpenOut, FuseReadIn, FuseReleaseIn, FuseSetattrIn, FuseWriteIn, FuseWriteOut,
        FATTR_ATIME, FATTR_CTIME, FATTR_FH, FATTR_GID, FATTR_LOCKOWNER, FATTR_MODE, FATTR_MTIME,
        FATTR_SIZE, FATTR_UID, FOPEN_KEEP_CACHE, FOPEN_NOFLUSH, FUSE_FLUSH, FUSE_FSYNC,
        FUSE_FSYNCDIR, FUSE_FSYNC_FDATASYNC, FUSE_HANDLE_KILLPRIV, FUSE_OPEN, FUSE_OPENDIR,
        FUSE_READ, FUSE_READ_LOCKOWNER, FUSE_SETATTR, FUSE_WRITE, FUSE_WRITEBACK_CACHE,
        FUSE_WRITE_CACHE,
    },
    reply::FuseReadPagesReply,
};
use super::FuseNode;

#[derive(Debug)]
struct FusePageCacheBackend {
    node: Weak<FuseNode>,
}

struct FillPagesFileCtx {
    file_size: usize,
    fh: u64,
    file_flags: u32,
    lifetime: Arc<FuseOpenLifetime>,
}

impl FusePageCacheBackend {
    fn new(node: Weak<FuseNode>) -> Self {
        Self { node }
    }
}

impl PageCacheBackend for FusePageCacheBackend {
    fn read_page(&self, _index: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        Err(SystemError::EIO)
    }

    fn write_page(&self, index: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let node = self.node.upgrade().ok_or(SystemError::EIO)?;
        node.writeback_page_with_handle(index, buf)
    }

    fn npages(&self) -> usize {
        let Some(node) = self.node.upgrade() else {
            return 0;
        };
        let Some(md) = node.cached_metadata_snapshot() else {
            return 0;
        };
        let size = md.size.max(0) as usize;
        if size == 0 {
            0
        } else {
            (size + MMArch::PAGE_SIZE - 1) >> MMArch::PAGE_SHIFT
        }
    }
}

impl FuseNode {
    pub(crate) fn with_writeback_admission<T>(&self, f: impl FnOnce() -> T) -> T {
        let _guard = self.writeback_barrier.read();
        f()
    }

    pub(crate) fn note_mmap_write(&self) {
        if !self.conn().has_init_flag(FUSE_WRITEBACK_CACHE) {
            return;
        }
        let now = PosixTimeSpec::now();
        if let Some(md) = self.cached_metadata.lock().as_mut() {
            md.mtime = now;
            md.ctime = now;
            self.bump_attr_version();
        }
    }

    pub(super) fn writeback_cache_write(
        &self,
        offset: usize,
        data: &[u8],
        open: &FuseOpenPrivateData,
    ) -> Result<usize, SystemError> {
        if data.is_empty() {
            return Ok(0);
        }
        let end = offset
            .checked_add(data.len())
            .ok_or(SystemError::EOVERFLOW)?;
        let page_cache = self.ensure_page_cache()?;
        let file_size = self.cached_or_fetch_metadata()?.size.max(0) as usize;
        let start_page = offset >> MMArch::PAGE_SHIFT;
        let end_page = (end - 1) >> MMArch::PAGE_SHIFT;

        // Keep the open request context alive while partial pages are filled.
        let _lifetime = open.lifetime.clone();
        for page_index in start_page..=end_page {
            let page_start = page_index << MMArch::PAGE_SHIFT;
            let write_start = core::cmp::max(offset, page_start);
            let write_end = core::cmp::min(end, page_start + MMArch::PAGE_SIZE);
            let full_overwrite =
                write_start == page_start && write_end - write_start == MMArch::PAGE_SIZE;
            if full_overwrite {
                let _ = page_cache
                    .manager()
                    .commit_overwrite_for_write(page_index)?;
                continue;
            }
            if page_cache.manager().peek_page(page_index).is_some() {
                continue;
            }

            if page_start >= file_size {
                let _ = page_cache
                    .manager()
                    .commit_overwrite_for_write(page_index)?;
                continue;
            }

            self.check_not_stale()?;
            let generation = self.generation();
            if open.open_context.node_generation != 0
                && generation != 0
                && open.open_context.node_generation != generation
            {
                return Err(SystemError::ESTALE);
            }
            let _ = page_cache
                .manager()
                .commit_page_for_write_with(page_index, |idx, dst| {
                    self.read_page_with_open(idx, dst, open.fh, open.open_flags)
                })?;
        }

        // Linux fuse_write_end() publishes the extended i_size before marking
        // the page dirty. Preserve the same invariant: writeback calculates
        // the last-page length from cached metadata, so it must never observe
        // dirty data while the old EOF is still visible.
        let written = page_cache.write_with_before_dirty(offset, data, |written| {
            self.note_successful_write(offset, written)
        })?;
        Ok(written)
    }

    pub(super) fn max_pages_bytes(&self) -> usize {
        core::cmp::max(1, self.conn().max_pages()).saturating_mul(MMArch::PAGE_SIZE)
    }

    pub(super) fn ensure_page_cache(&self) -> Result<Arc<PageCache>, SystemError> {
        let mut guard = self.page_cache.lock();
        if let Some(cache) = guard.as_ref() {
            return Ok(cache.clone());
        }
        let node = self.self_ref.upgrade().ok_or(SystemError::EIO)?;
        let inode: Arc<dyn IndexNode> = node;
        let backend = Arc::new(FusePageCacheBackend::new(self.self_ref.clone()));
        let cache = PageCache::new(Some(Arc::downgrade(&inode)), Some(backend));
        *guard = Some(cache.clone());
        Ok(cache)
    }

    pub(super) fn cached_page_cache(&self) -> Option<Arc<PageCache>> {
        self.page_cache.lock().clone()
    }

    pub(crate) fn cached_metadata_snapshot(&self) -> Option<Metadata> {
        self.cached_metadata.lock().clone()
    }

    pub(super) fn invalidate_clean_page_cache(&self) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.unmap_mapping_pages(0, None)?;
            let _ = cache.manager().invalidate_all_clean();
        }
        Ok(())
    }

    pub(crate) fn truncate_page_cache(&self, new_size: usize) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.truncate(new_size)?;
        }
        Ok(())
    }

    pub(crate) fn discard_completed_pages_beyond(
        &self,
        target: &PageCacheReadDmaReservation,
        eof: usize,
    ) {
        let Some(cache) = self.cached_page_cache() else {
            return;
        };
        for descriptor in target.descriptors() {
            if descriptor.page_index().saturating_mul(MMArch::PAGE_SIZE) >= eof {
                let _ = cache.manager().discard_clean_page(descriptor.page_index());
            }
        }
    }

    pub(super) fn setattr_size(
        &self,
        len: usize,
        lock_owner: Option<u64>,
        fh: Option<u64>,
        truncate_metadata: Option<(&Metadata, SetMetadataMask)>,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.resolve_pending_short_read_truncate(len)?;
        // Drain once before taking the exclusive admission barrier to reduce
        // hold time, then drain again under the barrier to close the race with
        // buffered writes and page_mkwrite.
        self.sync_dirty_cached_pages()?;
        let _barrier = self.writeback_barrier.write();
        self.sync_dirty_cached_pages()?;
        let mut valid = FATTR_SIZE;
        if lock_owner.is_some() {
            valid |= FATTR_LOCKOWNER;
        }
        if fh.is_some() {
            valid |= FATTR_FH;
        }
        let mut mode = 0;
        let mut uid = 0;
        let mut gid = 0;
        let mut atime = 0;
        let mut mtime = 0;
        let mut ctime = 0;
        let mut atimensec = 0;
        let mut mtimensec = 0;
        let mut ctimensec = 0;
        if let Some((metadata, mask)) = truncate_metadata {
            let automatic = mask.contains(SetMetadataMask::WRITE_SIDE_EFFECT);
            let conn = self.conn();
            let daemon_handles_killpriv = conn.has_init_flag(FUSE_HANDLE_KILLPRIV);
            let trust_local_cmtime = conn.has_init_flag(FUSE_WRITEBACK_CACHE);

            // Linux sends truncate and its killpriv state in one SETATTR.
            // With HANDLE_KILLPRIV the daemon owns the mode transition.
            if mask.contains(SetMetadataMask::MODE) && !(automatic && daemon_handles_killpriv) {
                valid |= FATTR_MODE;
                mode = metadata.mode.bits();
            }
            if mask.contains(SetMetadataMask::UID) {
                valid |= FATTR_UID;
                uid = metadata.uid as u32;
            }
            if mask.contains(SetMetadataMask::GID) {
                valid |= FATTR_GID;
                gid = metadata.gid as u32;
            }
            if mask.contains(SetMetadataMask::ATIME) {
                valid |= FATTR_ATIME;
                atime = metadata.atime.tv_sec as u64;
                atimensec = metadata.atime.tv_nsec as u32;
            }
            // DragonOS does not currently negotiate WRITEBACK_CACHE. Match
            // Linux FUSE by accepting the daemon-returned cmtime for truncate
            // instead of sending a second SETATTR without the file handle.
            if mask.contains(SetMetadataMask::MTIME) && (!automatic || trust_local_cmtime) {
                valid |= FATTR_MTIME;
                mtime = metadata.mtime.tv_sec as u64;
                mtimensec = metadata.mtime.tv_nsec as u32;
            }
            if mask.contains(SetMetadataMask::CTIME) && (!automatic || trust_local_cmtime) {
                valid |= FATTR_CTIME;
                ctime = metadata.ctime.tv_sec as u64;
                ctimensec = metadata.ctime.tv_nsec as u32;
            }
        }
        let inarg = FuseSetattrIn {
            valid,
            padding: 0,
            fh: fh.unwrap_or(0),
            size: len as u64,
            lock_owner: lock_owner.unwrap_or(0),
            atime,
            mtime,
            ctime,
            atimensec,
            mtimensec,
            ctimensec,
            mode,
            unused4: 0,
            uid,
            gid,
            unused5: 0,
        };
        let payload = self
            .conn()
            .request(FUSE_SETATTR, self.nodeid, fuse_pack_struct(&inarg))?;
        let out: FuseAttrOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&out.attr);
        let new_size = md.size.max(0) as usize;
        self.set_cached_metadata_with_valid(md, out.attr_valid, out.attr_valid_nsec);
        self.truncate_page_cache(new_size)?;
        Ok(())
    }

    pub(crate) fn note_short_read_eof(
        &self,
        page_index: usize,
        read_len: usize,
        observed_size: usize,
        observed_attr_version: u64,
    ) -> Result<(usize, bool), SystemError> {
        // Linux fuse_short_read() treats a short daemon read as a hole when
        // writeback-cache is active. Local i_size may include dirty sparse
        // extensions that the daemon has not observed yet, so shrinking it
        // here could discard valid dirty pages beyond the hole.
        if self.conn().has_init_flag(FUSE_WRITEBACK_CACHE) {
            return Ok((observed_size, false));
        }
        let eof = page_index
            .checked_mul(MMArch::PAGE_SIZE)
            .and_then(|start| start.checked_add(read_len))
            .ok_or(SystemError::EOVERFLOW)?;
        let mut should_truncate = false;
        {
            let mut guard = self.cached_metadata.lock();
            if let Some(md) = guard.as_mut() {
                let current_size = md.size.max(0) as usize;
                let current_version = self.attr_version();
                let first_from_snapshot =
                    current_version == observed_attr_version && current_size == observed_size;
                let continues_short_read_chain =
                    self.short_read_source_attr_version.load(Ordering::Acquire)
                        == observed_attr_version
                        && self.short_read_chain_attr_version.load(Ordering::Acquire)
                            == current_version
                        && current_size <= observed_size;
                if (first_from_snapshot || continues_short_read_chain) && eof < current_size {
                    md.size = eof as i64;
                    let chain_version = self.bump_attr_version();
                    self.short_read_source_attr_version
                        .store(observed_attr_version, Ordering::Release);
                    self.short_read_chain_attr_version
                        .store(chain_version, Ordering::Release);
                    self.pending_short_read_eof
                        .fetch_min(eof as u64, Ordering::AcqRel);
                    self.cached_metadata_deadline_ns
                        .store(u64::MAX, Ordering::Relaxed);
                    if let Some(cache) = self.cached_page_cache() {
                        let start_page = eof / MMArch::PAGE_SIZE;
                        schedule_work(Work::new(move || {
                            if let Err(error) = cache.unmap_mapping_pages_even_cow(start_page, None)
                            {
                                log::warn!(
                                    "fuse: short-read mapping invalidation failed eof={} err={:?}",
                                    eof,
                                    error
                                );
                            }
                        }));
                    }
                    should_truncate = true;
                }
            }
        }
        Ok((eof, should_truncate))
    }

    /// Finish an inode-wide page-cache truncate only when a later metadata
    /// refresh grows the file past an EOF established by a short READ.
    ///
    /// Running `PageCache::truncate()` from the FUSE reply completion would
    /// deadlock a single-threaded daemon when it waits for another outstanding
    /// readahead page. Deferring it to the next operation that can expose or
    /// create data past that EOF preserves coherence without blocking the
    /// daemon's reply path.
    pub(super) fn resolve_pending_short_read_truncate(
        &self,
        visible_size: usize,
    ) -> Result<(), SystemError> {
        let eof = self.pending_short_read_eof.load(Ordering::Acquire);
        if eof == u64::MAX || visible_size <= eof as usize {
            return Ok(());
        }

        self.truncate_pending_short_read_eof(eof)?;
        Ok(())
    }

    fn truncate_pending_short_read_eof(&self, eof: u64) -> Result<(), SystemError> {
        self.truncate_page_cache(eof as usize)?;
        let _ = self.pending_short_read_eof.compare_exchange(
            eof,
            u64::MAX,
            Ordering::AcqRel,
            Ordering::Acquire,
        );
        Ok(())
    }

    /// Linux `fuse_send_open()` forwards file flags except creation-only bits.
    fn fuse_open_in_flags(&self, raw: u32) -> u32 {
        let mut flags = raw
            & !(FileFlags::O_CREAT.bits() | FileFlags::O_EXCL.bits() | FileFlags::O_NOCTTY.bits());
        if !self
            .conn
            .has_init_flag(super::super::protocol::FUSE_ATOMIC_O_TRUNC)
        {
            flags &= !FileFlags::O_TRUNC.bits();
        }
        flags
    }

    pub(super) fn fuse_file_private_snapshot(
        data: &FilePrivateData,
    ) -> Result<FuseOpenPrivateData, SystemError> {
        match data {
            FilePrivateData::Fuse(FuseFilePrivateData::File(p)) => Ok(p.clone()),
            _ => Err(SystemError::EBADF),
        }
    }

    fn open_flags_are_writable(open_flags: u32) -> bool {
        let flags = FileFlags::from_bits_truncate(open_flags);
        let access = flags.access_flags();
        access == FileFlags::O_WRONLY || access == FileFlags::O_RDWR
    }

    fn register_writeback_handle(
        &self,
        open_flags: u32,
        fh: u64,
        fopen_flags: u32,
        no_open: bool,
        open_context: FuseOpenContext,
    ) -> Option<Arc<FuseWritebackHandle>> {
        if !Self::open_flags_are_writable(open_flags) {
            return None;
        }
        let handle = Arc::new(FuseWritebackHandle::new(
            fh,
            open_flags,
            fopen_flags,
            no_open,
            open_context,
        ));
        self.writeback_handles.lock().push(handle.clone());
        Some(handle)
    }

    pub(super) fn unregister_writeback_handle(&self, handle: &Arc<FuseWritebackHandle>) {
        handle.mark_closing();
        self.writeback_handles
            .lock()
            .retain(|candidate| !Arc::ptr_eq(candidate, handle));
        handle.wait_inflight_zero();
    }

    pub(crate) fn sync_cached_pages(&self) -> Result<(), SystemError> {
        if let Some(page_cache) = self.cached_page_cache() {
            page_cache.manager().sync()?;
        }
        Ok(())
    }

    pub(super) fn sync_dirty_cached_pages(&self) -> Result<(), SystemError> {
        let Some(page_cache) = self.cached_page_cache() else {
            return Ok(());
        };
        page_cache.manager().sync()
    }

    pub(super) fn check_and_advance_open_wb_error(
        &self,
        data: &FuseOpenPrivateData,
    ) -> Result<(), SystemError> {
        let Some(page_cache) = self.cached_page_cache() else {
            return Ok(());
        };
        let mut since = data.open_context.wb_errseq.lock();
        match page_cache.check_and_advance_writeback_error(&mut since) {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub(super) fn flush_open_file(
        &self,
        data: &FuseOpenPrivateData,
        lock_owner: u64,
    ) -> Result<(), SystemError> {
        // Linux fuse_flush() drains inode writeback even when this particular
        // descriptor is read-only: another writable open may have admitted
        // dirty data for the same inode.
        let writeback_cache = self.conn().has_init_flag(FUSE_WRITEBACK_CACHE);
        let _barrier = writeback_cache.then(|| self.writeback_barrier.write());
        let writeback_result = if writeback_cache {
            self.sync_dirty_cached_pages()
        } else {
            Ok(())
        };
        let wb_error_result = self.check_and_advance_open_wb_error(data);

        writeback_result?;
        wb_error_result?;

        if data.no_open || (data.fopen_flags & FOPEN_NOFLUSH) != 0 || self.conn().no_flush() {
            return Ok(());
        }

        let flush_in = FuseFlushIn {
            fh: data.fh,
            unused: 0,
            padding: 0,
            lock_owner,
        };
        match self
            .conn()
            .request(FUSE_FLUSH, self.nodeid, fuse_pack_struct(&flush_in))
        {
            Err(SystemError::ENOSYS) => {
                self.conn().mark_no_flush();
                Ok(())
            }
            result => result.map(|_| ()),
        }
    }

    pub(crate) fn pin_writeback_handle(
        &self,
    ) -> Result<super::super::private_data::FuseWritebackHandlePin, SystemError> {
        let handles = self.writeback_handles.lock().clone();
        for handle in handles {
            if let Some(pin) = handle.try_pin() {
                return Ok(pin);
            }
        }
        Err(SystemError::EIO)
    }

    fn writeback_page_with_handle(
        &self,
        page_index: usize,
        buf: &[u8],
    ) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        let pin = self.pin_writeback_handle()?;
        let handle = pin.handle();
        let current_generation = self.generation();
        if handle.open_context.node_generation != 0
            && current_generation != 0
            && handle.open_context.node_generation != current_generation
        {
            return Err(SystemError::ESTALE);
        }
        let max_write = core::cmp::min(self.conn().max_write(), self.max_pages_bytes());
        if max_write == 0 {
            return Err(SystemError::EIO);
        }

        let base_offset = page_index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let mut total = 0usize;
        while total < buf.len() {
            self.check_not_stale()?;
            let current_generation = self.generation();
            if handle.open_context.node_generation != 0
                && current_generation != 0
                && handle.open_context.node_generation != current_generation
            {
                return Err(SystemError::ESTALE);
            }

            let chunk = core::cmp::min(max_write, buf.len() - total);
            let offset = base_offset
                .checked_add(total)
                .ok_or(SystemError::EOVERFLOW)?;
            let write_in = FuseWriteIn {
                fh: handle.fh,
                offset: offset as u64,
                size: chunk as u32,
                write_flags: FUSE_WRITE_CACHE,
                lock_owner: 0,
                flags: 0,
                padding: 0,
            };
            let mut payload_in = Vec::with_capacity(size_of::<FuseWriteIn>() + chunk);
            payload_in.extend_from_slice(fuse_pack_struct(&write_in));
            payload_in.extend_from_slice(&buf[total..total + chunk]);
            let payload = self.conn().request_with_cred(
                FUSE_WRITE,
                self.nodeid,
                &payload_in,
                handle.open_context.request_cred,
            )?;
            let out: FuseWriteOut = fuse_read_struct(&payload)?;
            if out.size as usize != chunk {
                return Err(SystemError::EIO);
            }
            total += chunk;
        }

        Ok(total)
    }

    fn set_open_private_data(
        &self,
        data: &mut FilePrivateData,
        opcode: u32,
        fh: u64,
        open_flags: u32,
        fopen_flags: u32,
        no_open: bool,
    ) -> Result<(), SystemError> {
        let conn_any: Arc<dyn core::any::Any + Send + Sync> = self.conn.clone();
        let node = self.self_ref.upgrade().ok_or(SystemError::EIO)?;
        let open_context = FuseOpenContext {
            request_cred: FuseRequestCred::from_current(),
            lock_owner: crate::filesystem::vfs::vcore::current_file_lock_owner_id(),
            node_generation: self.generation(),
            wb_errseq: Arc::new(Mutex::new(
                self.cached_page_cache()
                    .map(|page_cache| page_cache.sample_writeback_error())
                    .unwrap_or(0),
            )),
        };
        let writeback_handle = if opcode == FUSE_OPEN {
            self.register_writeback_handle(
                open_flags,
                fh,
                fopen_flags,
                no_open,
                open_context.clone(),
            )
        } else {
            None
        };
        *data = match opcode {
            FUSE_OPEN => FilePrivateData::Fuse(FuseFilePrivateData::File(FuseOpenPrivateData {
                conn: conn_any,
                node: node.clone(),
                fh,
                open_flags,
                fopen_flags,
                no_open,
                open_context,
                writeback_handle,
                readahead_state: Arc::new(Mutex::new(FileReadaheadState::new())),
                lifetime: Arc::new(FuseOpenLifetime::new()),
            })),
            FUSE_OPENDIR => FilePrivateData::Fuse(FuseFilePrivateData::Dir(FuseOpenPrivateData {
                conn: conn_any,
                node,
                fh,
                open_flags,
                fopen_flags,
                no_open,
                open_context,
                writeback_handle: None,
                readahead_state: Arc::new(Mutex::new(FileReadaheadState::new())),
                lifetime: Arc::new(FuseOpenLifetime::new()),
            })),
            _ => return Err(SystemError::EINVAL),
        };
        Ok(())
    }

    fn finish_open_cache_state(
        &self,
        opcode: u32,
        flags: &FileFlags,
        fopen_flags: u32,
    ) -> Result<(), SystemError> {
        if opcode != FUSE_OPEN {
            return Ok(());
        }

        if flags.contains(FileFlags::O_TRUNC)
            && self
                .conn
                .has_init_flag(super::super::protocol::FUSE_ATOMIC_O_TRUNC)
        {
            self.truncate_page_cache(0)?;
            let mut guard = self.cached_metadata.lock();
            if let Some(md) = guard.as_mut() {
                md.size = 0;
                self.bump_attr_version();
            }
        } else if (fopen_flags & FOPEN_KEEP_CACHE) == 0 {
            self.invalidate_clean_page_cache()?;
        }

        Ok(())
    }

    pub(super) fn open_common(
        &self,
        opcode: u32,
        data: &mut FilePrivateData,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let file_flags = flags.bits();
        if self.conn.should_skip_open(opcode) {
            self.finish_open_cache_state(opcode, flags, FOPEN_KEEP_CACHE)?;
            return self.set_open_private_data(data, opcode, 0, file_flags, FOPEN_KEEP_CACHE, true);
        }

        let fuse_open_flags = self.fuse_open_in_flags(file_flags);
        let open_in = FuseOpenIn {
            flags: fuse_open_flags,
            open_flags: 0,
        };
        let payload = match self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&open_in))
        {
            Ok(v) => v,
            Err(SystemError::ENOSYS) if self.conn.open_enosys_is_supported(opcode) => {
                self.conn.mark_no_open(opcode);
                self.finish_open_cache_state(opcode, flags, FOPEN_KEEP_CACHE)?;
                return self.set_open_private_data(
                    data,
                    opcode,
                    0,
                    file_flags,
                    FOPEN_KEEP_CACHE,
                    true,
                );
            }
            Err(e) => return Err(e),
        };
        let out: FuseOpenOut = fuse_read_struct(&payload)?;
        self.finish_open_cache_state(opcode, flags, out.open_flags)?;
        self.set_open_private_data(data, opcode, out.fh, file_flags, out.open_flags, false)
    }

    pub(super) fn release_common_for_node(
        &self,
        opcode: u32,
        nodeid: u64,
        fh: u64,
        file_flags: u32,
        lock_owner: u64,
    ) {
        let inarg = FuseReleaseIn {
            fh,
            flags: file_flags,
            release_flags: 0,
            lock_owner,
        };
        let conn = self.conn.clone();
        let payload = fuse_pack_struct(&inarg).to_vec();
        if let Err(err) = conn.request_nocreds_background(opcode, nodeid, &payload) {
            log::warn!(
                "fuse: queue async release failed opcode={} nodeid={} fh={} err={:?}",
                opcode,
                nodeid,
                fh,
                err
            );
        }
    }

    pub(super) fn release_common(&self, opcode: u32, fh: u64, file_flags: u32, lock_owner: u64) {
        self.release_common_for_node(opcode, self.nodeid, fh, file_flags, lock_owner);
    }

    pub(super) fn ensure_regular(&self) -> Result<(), SystemError> {
        let md = self.cached_or_fetch_metadata()?;
        if md.file_type != FileType::File {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    pub(super) fn fsync_common(&self, datasync: bool) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let md = self.cached_or_fetch_metadata()?;
        let opcode = match md.file_type {
            FileType::File => FUSE_FSYNC,
            FileType::Dir => FUSE_FSYNCDIR,
            _ => return Ok(()),
        };
        if self.conn().no_fsync(opcode) {
            return Ok(());
        }
        let inarg = FuseFsyncIn {
            fh: 0,
            fsync_flags: if datasync { FUSE_FSYNC_FDATASYNC } else { 0 },
            padding: 0,
        };
        match self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&inarg))
        {
            Err(SystemError::ENOSYS) => {
                self.conn().mark_no_fsync(opcode);
                Ok(())
            }
            result => result.map(|_| ()),
        }
    }

    pub(super) fn fsync_with_fh(
        &self,
        opcode: u32,
        fh: u64,
        datasync: bool,
    ) -> Result<(), SystemError> {
        if self.conn().no_fsync(opcode) {
            return Ok(());
        }
        let inarg = FuseFsyncIn {
            fh,
            fsync_flags: if datasync { FUSE_FSYNC_FDATASYNC } else { 0 },
            padding: 0,
        };
        match self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&inarg))
        {
            Err(SystemError::ENOSYS) => {
                self.conn().mark_no_fsync(opcode);
                Ok(())
            }
            result => result.map(|_| ()),
        }
    }

    pub(super) fn read_direct_with_open(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        fh: u64,
        file_flags: u32,
        lock_owner: u64,
    ) -> Result<usize, SystemError> {
        let max_read = core::cmp::min(self.conn().max_read(), self.max_pages_bytes());
        if max_read == 0 {
            return Err(SystemError::EIO);
        }

        let mut total_read = 0usize;
        while total_read < len {
            let chunk = core::cmp::min(max_read, len - total_read);
            let Some(chunk_offset) = offset.checked_add(total_read) else {
                return if total_read > 0 {
                    Ok(total_read)
                } else {
                    Err(SystemError::EOVERFLOW)
                };
            };
            if chunk_offset > i64::MAX as usize {
                return if total_read > 0 {
                    Ok(total_read)
                } else {
                    Err(SystemError::EINVAL)
                };
            }
            let read_in = FuseReadIn {
                fh,
                offset: chunk_offset as u64,
                size: chunk as u32,
                read_flags: if lock_owner != 0 {
                    FUSE_READ_LOCKOWNER
                } else {
                    0
                },
                lock_owner,
                flags: file_flags,
                padding: 0,
            };
            let payload =
                match self
                    .conn()
                    .request(FUSE_READ, self.nodeid, fuse_pack_struct(&read_in))
                {
                    Ok(payload) => payload,
                    Err(_) if total_read > 0 => return Ok(total_read),
                    Err(err) => return Err(err),
                };
            if payload.len() > chunk {
                return if total_read > 0 {
                    Ok(total_read)
                } else {
                    Err(SystemError::EIO)
                };
            }
            let n = payload.len();
            buf[total_read..total_read + n].copy_from_slice(&payload);
            total_read += n;
            if n < chunk {
                break;
            }
        }

        Ok(total_read)
    }

    fn read_page_with_open(
        &self,
        page_index: usize,
        page_buf: &mut [u8],
        fh: u64,
        file_flags: u32,
    ) -> Result<usize, SystemError> {
        page_buf.fill(0);
        self.read_direct_with_open(
            page_index
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?,
            MMArch::PAGE_SIZE,
            page_buf,
            fh,
            file_flags,
            0,
        )
    }

    fn fill_page_cache_range_with_open(
        &self,
        page_cache: &Arc<PageCache>,
        start_page: usize,
        end_page: usize,
        demand_end_page: usize,
        file_ctx: &FillPagesFileCtx,
    ) -> Result<(usize, Option<usize>), SystemError> {
        if start_page >= end_page || file_ctx.file_size == 0 {
            return Ok((0, None));
        }

        let max_read = self.conn().max_read();
        let max_pages_by_read = core::cmp::max(1, max_read >> MMArch::PAGE_SHIFT);
        let max_pages = core::cmp::max(
            1,
            core::cmp::min(max_pages_by_read, self.conn().max_pages()),
        );
        let mut total_read = 0usize;
        let truncate_eof = None;
        let mut pending_reads = Vec::new();
        let mut submission_error = None;
        let observed_attr_version = self.attr_version();
        let mut submitted_requests = 0usize;

        let mut idx = start_page;
        while idx < end_page {
            if page_cache.is_page_ready(idx) {
                idx += 1;
                continue;
            }

            let run_start = idx;
            let mut run_end = run_start + 1;
            while run_end < end_page
                && run_end - run_start < max_pages
                && !page_cache.is_page_ready(run_end)
            {
                run_end += 1;
            }

            let read_offset = run_start
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?;
            if read_offset >= file_ctx.file_size {
                break;
            }

            let read_pages_len = (run_end - run_start)
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?;
            let read_len = core::cmp::min(
                core::cmp::min(read_pages_len, max_read),
                file_ctx.file_size - read_offset,
            );
            if read_len == 0 {
                break;
            }

            let speculative = run_start >= demand_end_page;
            let target = match page_cache
                .manager()
                .reserve_read_dma(run_start, run_end - run_start)
            {
                Ok(target) => Arc::new(target),
                Err(SystemError::EEXIST) => {
                    if speculative {
                        // The foreground read has no dependency on a conflicting
                        // speculative window. Do not inherit its latency or error.
                        break;
                    }
                    // A concurrent reader won the reservation race. Wait for its first page to
                    // leave Loading, then rebuild the missing run from current cache state.
                    drop(page_cache.manager().commit_page(run_start)?);
                    continue;
                }
                Err(error) => return Err(error),
            };
            let read_in = FuseReadIn {
                fh: file_ctx.fh,
                offset: read_offset as u64,
                size: read_len as u32,
                read_flags: 0,
                lock_owner: 0,
                flags: file_ctx.file_flags,
                padding: 0,
            };
            let Some(open_pin) = file_ctx.lifetime.try_pin() else {
                let _ = target.rollback(SystemError::EIO);
                if !speculative {
                    submission_error = Some(SystemError::EIO);
                }
                break;
            };
            let pending = self.conn().enqueue_background_read_pages(
                self.nodeid,
                fuse_pack_struct(&read_in),
                FuseRequestCred::from_current(),
                speculative,
                BackgroundReadPagesCtx {
                    destination: target.clone(),
                    node: self.self_ref.clone(),
                    start_page: run_start,
                    requested: read_len,
                    observed_size: file_ctx.file_size,
                    observed_attr_version,
                    open_pin,
                },
            );
            let pending = match pending {
                Ok(Some(pending)) => pending,
                Ok(None) => {
                    let _ = target.rollback(SystemError::EIO);
                    break;
                }
                Err(error) => {
                    let _ = target.rollback(error.clone());
                    if !speculative {
                        submission_error = Some(error);
                    }
                    break;
                }
            };
            submitted_requests += 1;
            if !speculative {
                pending_reads.push((read_len, pending));
            }
            idx = run_end;
        }

        let mut first_error = submission_error;
        let mut interrupted = false;
        super::super::stats::on_readahead_batch(
            end_page.saturating_sub(start_page),
            submitted_requests,
        );
        for (read_len, pending) in pending_reads {
            let (result, was_interrupted) = self.conn().wait_background_read_pages(&pending);
            interrupted |= was_interrupted;
            let result = match result {
                Ok(result) => result,
                Err(error) => {
                    if first_error.is_none() {
                        first_error = Some(error);
                    }
                    if interrupted {
                        break;
                    }
                    continue;
                }
            };
            let bytes_read = match result {
                FuseReadPagesReply::Direct { bytes } => bytes,
                FuseReadPagesReply::Contiguous(reply) => reply.len(),
            };
            if bytes_read > read_len {
                return Err(SystemError::EIO);
            }
            total_read += bytes_read.div_ceil(MMArch::PAGE_SIZE);
        }

        if let Some(error) = first_error {
            return Err(error);
        }
        if interrupted {
            return Err(SystemError::EINTR);
        }

        Ok((total_read, truncate_eof))
    }

    pub(super) fn read_cached_with_open(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        open: &FuseOpenPrivateData,
    ) -> Result<usize, SystemError> {
        let fh = open.fh;
        let file_flags = open.open_flags;
        let ra_state = &open.readahead_state;
        let md = self.cached_or_fetch_metadata()?;
        let file_size = md.size.max(0) as usize;
        self.resolve_pending_short_read_truncate(file_size)?;
        if offset >= file_size || len == 0 {
            return Ok(0);
        }

        let read_len = core::cmp::min(len, file_size - offset);
        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + read_len - 1) >> MMArch::PAGE_SHIFT;
        let page_cache = self.ensure_page_cache()?;
        let _invalidate = page_cache.invalidate_read();
        let last_file_page = (file_size - 1) >> MMArch::PAGE_SHIFT;
        let max_window = core::cmp::max(
            1,
            core::cmp::min(
                self.conn().max_readahead_pages(),
                FileReadaheadState::new().ra_pages,
            ),
        );
        let demand_pages = end_page_index - start_page_index + 1;
        let readaround_pages = {
            let mut state = ra_state.lock();
            let sequential = start_page_index == 0 || state.first_sequential(start_page_index);
            let pages = if sequential {
                let base = core::cmp::max(demand_pages, state.size.max(1));
                core::cmp::min(
                    max_window,
                    core::cmp::max(demand_pages, base.saturating_mul(2)),
                )
            } else {
                demand_pages
            };
            state.start = start_page_index;
            state.size = pages;
            state.async_size = pages.saturating_sub(demand_pages);
            state.prev_index = end_page_index as i64;
            pages
        };
        let prefetch_end = core::cmp::min(
            last_file_page + 1,
            core::cmp::max(
                end_page_index + 1,
                start_page_index.saturating_add(readaround_pages),
            ),
        );
        let file_ctx = FillPagesFileCtx {
            file_size,
            fh,
            file_flags,
            lifetime: open.lifetime.clone(),
        };
        let (_, mut truncate_eof) = self.fill_page_cache_range_with_open(
            &page_cache,
            start_page_index,
            prefetch_end,
            end_page_index + 1,
            &file_ctx,
        )?;
        let observed_attr_version = self.attr_version();

        let mut dst_offset = 0usize;
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index << MMArch::PAGE_SHIFT;
            let page_end = page_start + MMArch::PAGE_SIZE;
            let copy_start = core::cmp::max(offset, page_start);
            let copy_end = core::cmp::min(offset + read_len, page_end);
            let copy_len = copy_end.saturating_sub(copy_start);
            if copy_len == 0 {
                continue;
            }

            let mut filled_len = None;
            let page = page_cache
                .manager()
                .commit_page_with(page_index, |idx, dst| {
                    let read_len = self.read_page_with_open(idx, dst, fh, file_flags)?;
                    filled_len = Some(read_len);
                    Ok(read_len)
                })?;

            let mut copy_end = copy_end;
            if let Some(read_len) = filled_len {
                if read_len < MMArch::PAGE_SIZE {
                    let (eof, should_truncate) = self.note_short_read_eof(
                        page_index,
                        read_len,
                        file_size,
                        observed_attr_version,
                    )?;
                    copy_end = core::cmp::min(copy_end, eof);
                    if should_truncate {
                        truncate_eof = Some(eof);
                    }
                }
            }

            let current_size = self
                .cached_metadata_snapshot()
                .map(|md| md.size.max(0) as usize)
                .unwrap_or(file_size);
            copy_end = core::cmp::min(copy_end, current_size);
            if copy_start >= copy_end {
                break;
            }

            let copy_len = copy_end.saturating_sub(copy_start);
            let page_guard = page.read();
            let page_offset = copy_start - page_start;
            unsafe {
                buf[dst_offset..dst_offset + copy_len]
                    .copy_from_slice(&page_guard.as_slice()[page_offset..page_offset + copy_len]);
            }
            dst_offset += copy_len;

            if (!self.conn().has_init_flag(FUSE_WRITEBACK_CACHE)
                && filled_len.is_some_and(|read_len| read_len < MMArch::PAGE_SIZE))
                || current_size <= page_end
            {
                break;
            }
        }
        drop(_invalidate);

        if let Some(eof) = truncate_eof {
            if matches!(
                self.cached_metadata_snapshot(),
                Some(md) if md.size.max(0) as usize == eof
            ) {
                self.truncate_page_cache(eof)?;
            }
        }

        Ok(dst_offset)
    }

    pub(crate) fn fault_page_with_open(
        &self,
        page_index: usize,
        fh: u64,
        file_flags: u32,
    ) -> Result<Arc<crate::mm::page::Page>, SystemError> {
        let md = self.cached_metadata_snapshot().ok_or(SystemError::EIO)?;
        let file_size = md.size.max(0) as usize;
        self.resolve_pending_short_read_truncate(file_size)?;
        if file_size == 0 || page_index.saturating_mul(MMArch::PAGE_SIZE) >= file_size {
            return Err(SystemError::EINVAL);
        }
        let page_cache = self.ensure_page_cache()?;
        let observed_attr_version = self.attr_version();
        let mut filled_len = None;
        let page = page_cache
            .manager()
            .commit_page_with(page_index, |idx, dst| {
                let read_len = self.read_page_with_open(idx, dst, fh, file_flags)?;
                filled_len = Some(read_len);
                Ok(read_len)
            })?;
        if let Some(read_len) = filled_len {
            if read_len < MMArch::PAGE_SIZE {
                let (eof, _) = self.note_short_read_eof(
                    page_index,
                    read_len,
                    file_size,
                    observed_attr_version,
                )?;
                if page_index.saturating_mul(MMArch::PAGE_SIZE) >= eof {
                    drop(page);
                    page_cache.manager().discard_clean_page(page_index)?;
                    return Err(SystemError::EINVAL);
                }
            }
        }
        let current_size = self
            .cached_metadata_snapshot()
            .map(|md| md.size.max(0) as usize)
            .unwrap_or(file_size);
        if page_index.saturating_mul(MMArch::PAGE_SIZE) >= current_size {
            drop(page);
            page_cache.manager().discard_clean_page(page_index)?;
            return Err(SystemError::EINVAL);
        }
        Ok(page)
    }

    pub(crate) fn mmap_readahead_with_open(
        &self,
        page_index: usize,
        req_pages: usize,
        ra_state: &mut FileReadaheadState,
        fh: u64,
        file_flags: u32,
        lifetime: Arc<FuseOpenLifetime>,
    ) -> Result<usize, SystemError> {
        if req_pages == 0 {
            return Ok(0);
        }

        let md = self.cached_metadata_snapshot().ok_or(SystemError::EIO)?;
        let file_size = md.size.max(0) as usize;
        self.resolve_pending_short_read_truncate(file_size)?;
        if file_size == 0 {
            return Ok(0);
        }

        let last_file_page = (file_size - 1) >> MMArch::PAGE_SHIFT;
        if page_index > last_file_page {
            return Ok(0);
        }

        let page_cache = self.ensure_page_cache()?;
        let max_pages_by_read = core::cmp::max(1, self.conn().max_read() >> MMArch::PAGE_SHIFT);
        let max_pages_by_conn = core::cmp::min(max_pages_by_read, self.conn().max_pages());
        let max_pages = core::cmp::max(
            1,
            core::cmp::min(
                ra_state.ra_pages,
                core::cmp::min(max_pages_by_conn, self.conn().max_readahead_pages()),
            ),
        );
        let pages_to_read = core::cmp::min(max_pages, core::cmp::max(req_pages, 16));
        let end_page = core::cmp::min(last_file_page + 1, page_index.saturating_add(pages_to_read));

        let file_ctx = FillPagesFileCtx {
            file_size,
            fh,
            file_flags,
            lifetime,
        };
        let (total_read, truncate_eof) = self.fill_page_cache_range_with_open(
            &page_cache,
            page_index,
            end_page,
            page_index.saturating_add(req_pages),
            &file_ctx,
        )?;

        if let Some(eof) = truncate_eof {
            if matches!(
                self.cached_metadata_snapshot(),
                Some(md) if md.size.max(0) as usize == eof
            ) {
                self.truncate_page_cache(eof)?;
            }
        }

        ra_state.start = page_index;
        ra_state.size = end_page.saturating_sub(page_index);
        ra_state.async_size = ra_state.size.saturating_sub(req_pages);
        ra_state.prev_index = end_page.saturating_sub(1) as i64;

        Ok(total_read)
    }

    pub(super) fn update_cached_pages_after_write(
        &self,
        page_cache: &Arc<PageCache>,
        offset: usize,
        data: &[u8],
    ) -> Result<(), SystemError> {
        if data.is_empty() {
            return Ok(());
        }
        let end = offset
            .checked_add(data.len())
            .ok_or(SystemError::EOVERFLOW)?;

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (end - 1) >> MMArch::PAGE_SHIFT;
        for page_index in start_page_index..=end_page_index {
            let page_start = page_index << MMArch::PAGE_SHIFT;
            let page_end = page_start + MMArch::PAGE_SIZE;
            let write_start = core::cmp::max(offset, page_start);
            let write_end = core::cmp::min(end, page_end);
            let write_len = write_end.saturating_sub(write_start);
            if write_len == 0 {
                continue;
            }

            let src_offset = write_start - offset;
            let page_offset = write_start - page_start;
            let _ = page_cache.manager().update_ready_page(
                page_index,
                page_offset,
                &data[src_offset..src_offset + write_len],
            )?;
        }

        Ok(())
    }

    pub(super) fn prepare_cached_write_range(
        &self,
        page_cache: &Arc<PageCache>,
        offset: usize,
        len: usize,
    ) -> Result<(), SystemError> {
        let Some((start_page_index, end_page_index, _)) = Self::direct_io_page_range(offset, len)?
        else {
            return Ok(());
        };

        page_cache
            .manager()
            .wait_writeback_range(start_page_index, end_page_index)
    }

    pub(super) fn note_successful_write(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<(), SystemError> {
        if len == 0 {
            return Ok(());
        }
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let mut metadata = self.cached_metadata.lock();
        if let Some(md) = metadata.as_mut() {
            if end > md.size.max(0) as usize {
                md.size = end as i64;
            }
            if self.conn().has_init_flag(FUSE_WRITEBACK_CACHE) {
                let now = PosixTimeSpec::now();
                md.mtime = now;
                md.ctime = now;
            }
        }
        // Every successful write is a mutation fence for READ replies issued
        // from an older metadata snapshot, even when it does not extend size.
        self.bump_attr_version();
        Ok(())
    }

    pub(super) fn invalidate_cached_pages_after_direct_write(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<(), SystemError> {
        if len == 0 {
            return Ok(());
        }
        let Some(page_cache) = self.cached_page_cache() else {
            return Ok(());
        };
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (end - 1) >> MMArch::PAGE_SHIFT;
        let end_page_exclusive = end_page_index
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        let _invalidate = page_cache.invalidate_write();
        page_cache.unmap_mapping_pages(start_page_index, Some(end_page_exclusive))?;
        let _ = page_cache
            .manager()
            .discard_clean_range(start_page_index, end_page_index)?;
        Ok(())
    }

    fn direct_io_page_range(
        offset: usize,
        len: usize,
    ) -> Result<Option<(usize, usize, usize)>, SystemError> {
        if len == 0 {
            return Ok(None);
        }
        let end = offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (end - 1) >> MMArch::PAGE_SHIFT;
        let end_page_exclusive = end_page_index
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        Ok(Some((start_page_index, end_page_index, end_page_exclusive)))
    }

    pub(super) fn prepare_direct_io_range(
        &self,
        offset: usize,
        len: usize,
        data: &FuseOpenPrivateData,
        discard_clean: bool,
    ) -> Result<(), SystemError> {
        let Some((start_page_index, end_page_index, end_page_exclusive)) =
            Self::direct_io_page_range(offset, len)?
        else {
            return Ok(());
        };
        let Some(page_cache) = self.cached_page_cache() else {
            return Ok(());
        };

        page_cache
            .manager()
            .writeback_range(start_page_index, end_page_index)?;
        page_cache
            .manager()
            .wait_writeback_range(start_page_index, end_page_index)?;
        self.check_and_advance_open_wb_error(data)?;

        if discard_clean {
            let _invalidate = page_cache.invalidate_write();
            page_cache.unmap_mapping_pages(start_page_index, Some(end_page_exclusive))?;
            let _ = page_cache
                .manager()
                .discard_clean_range(start_page_index, end_page_index)?;
        }
        Ok(())
    }
}
