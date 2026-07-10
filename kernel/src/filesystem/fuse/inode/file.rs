use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::{mem::size_of, sync::atomic::Ordering};

use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::{
        page_cache::{PageCache, PageCacheBackend},
        vfs::{file::FileFlags, FilePrivateData, FileType, IndexNode, Metadata},
    },
    libs::mutex::Mutex,
    mm::{readahead::FileReadaheadState, MemoryManagementArch},
};

use super::super::{
    conn::FuseRequestCred,
    private_data::{
        FuseFilePrivateData, FuseOpenContext, FuseOpenPrivateData, FuseWritebackHandle,
    },
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAttrOut, FuseFlushIn, FuseFsyncIn, FuseOpenIn,
        FuseOpenOut, FuseReadIn, FuseReleaseIn, FuseSetattrIn, FuseWriteIn, FuseWriteOut, FATTR_FH,
        FATTR_LOCKOWNER, FATTR_SIZE, FOPEN_KEEP_CACHE, FOPEN_NOFLUSH, FUSE_FLUSH, FUSE_FSYNC,
        FUSE_FSYNCDIR, FUSE_FSYNC_FDATASYNC, FUSE_OPEN, FUSE_OPENDIR, FUSE_READ,
        FUSE_READ_LOCKOWNER, FUSE_SETATTR, FUSE_WRITE, FUSE_WRITE_CACHE,
    },
};
use super::FuseNode;

#[derive(Debug)]
struct FusePageCacheBackend {
    node: Weak<FuseNode>,
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

    fn cached_metadata_snapshot(&self) -> Option<Metadata> {
        self.cached_metadata.lock().clone()
    }

    pub(super) fn invalidate_clean_page_cache(&self) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.unmap_mapping_pages(0, None)?;
            let _ = cache.manager().invalidate_all_clean();
        }
        Ok(())
    }

    pub(super) fn truncate_page_cache(&self, new_size: usize) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.truncate(new_size)?;
        }
        Ok(())
    }

    pub(super) fn setattr_size(
        &self,
        len: usize,
        lock_owner: Option<u64>,
        fh: Option<u64>,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let mut valid = FATTR_SIZE;
        if lock_owner.is_some() {
            valid |= FATTR_LOCKOWNER;
        }
        if fh.is_some() {
            valid |= FATTR_FH;
        }
        let inarg = FuseSetattrIn {
            valid,
            padding: 0,
            fh: fh.unwrap_or(0),
            size: len as u64,
            lock_owner: lock_owner.unwrap_or(0),
            atime: 0,
            mtime: 0,
            ctime: 0,
            atimensec: 0,
            mtimensec: 0,
            ctimensec: 0,
            mode: 0,
            unused4: 0,
            uid: 0,
            gid: 0,
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

    fn note_short_read_eof(
        &self,
        page_index: usize,
        read_len: usize,
        observed_size: usize,
    ) -> Result<(usize, bool), SystemError> {
        let eof = page_index
            .checked_mul(MMArch::PAGE_SIZE)
            .and_then(|start| start.checked_add(read_len))
            .ok_or(SystemError::EOVERFLOW)?;
        let mut should_truncate = false;
        {
            let mut guard = self.cached_metadata.lock();
            if let Some(md) = guard.as_mut() {
                let current_size = md.size.max(0) as usize;
                if current_size == observed_size && eof < current_size {
                    md.size = eof as i64;
                    self.cached_metadata_deadline_ns
                        .store(u64::MAX, Ordering::Relaxed);
                    should_truncate = true;
                }
            }
        }
        Ok((eof, should_truncate))
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
        if !page_cache.has_dirty_pages() {
            return Ok(());
        }
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
        let writeback_result = if data.writeback_handle.is_some() {
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
            self.note_successful_write(offset, chunk)?;
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
        file_size: usize,
        fh: u64,
        file_flags: u32,
    ) -> Result<(usize, Option<usize>), SystemError> {
        if start_page >= end_page || file_size == 0 {
            return Ok((0, None));
        }

        let max_read = self.conn().max_read();
        let max_pages_by_read = core::cmp::max(1, max_read >> MMArch::PAGE_SHIFT);
        let max_pages = core::cmp::max(
            1,
            core::cmp::min(max_pages_by_read, self.conn().max_pages()),
        );
        let mut total_read = 0usize;
        let mut truncate_eof = None;

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
            if read_offset >= file_size {
                break;
            }

            let read_pages_len = (run_end - run_start)
                .checked_mul(MMArch::PAGE_SIZE)
                .ok_or(SystemError::EOVERFLOW)?;
            let read_len = core::cmp::min(
                core::cmp::min(read_pages_len, max_read),
                file_size - read_offset,
            );
            if read_len == 0 {
                break;
            }

            let mut read_buf = vec![0u8; read_len];
            let bytes_read = self.read_direct_with_open(
                read_offset,
                read_len,
                &mut read_buf,
                fh,
                file_flags,
                0,
            )?;
            if bytes_read == 0 {
                let (eof, should_truncate) = self.note_short_read_eof(run_start, 0, file_size)?;
                if should_truncate {
                    truncate_eof = Some(eof);
                }
                break;
            }

            let covered_pages = bytes_read.div_ceil(MMArch::PAGE_SIZE);
            let pages_to_commit = core::cmp::min(run_end - run_start, covered_pages);
            let mut saw_short_page = false;
            for rel_page in 0..pages_to_commit {
                let page_idx = run_start + rel_page;
                let page_offset = rel_page * MMArch::PAGE_SIZE;
                let page_read_len =
                    core::cmp::min(MMArch::PAGE_SIZE, bytes_read.saturating_sub(page_offset));
                if page_read_len == 0 {
                    break;
                }

                let mut filled_len = None;
                let page = page_cache.manager().commit_page_with(page_idx, |_, dst| {
                    dst.fill(0);
                    dst[..page_read_len]
                        .copy_from_slice(&read_buf[page_offset..page_offset + page_read_len]);
                    filled_len = Some(page_read_len);
                    Ok(page_read_len)
                })?;
                drop(page);

                if filled_len.is_some() {
                    total_read += 1;
                }

                if page_read_len < MMArch::PAGE_SIZE {
                    let (eof, should_truncate) =
                        self.note_short_read_eof(page_idx, page_read_len, file_size)?;
                    if should_truncate {
                        truncate_eof = Some(eof);
                    }
                    saw_short_page = true;
                    break;
                }
            }

            if bytes_read < read_len && !saw_short_page {
                let (eof, should_truncate) =
                    self.note_short_read_eof(run_start, bytes_read, file_size)?;
                if should_truncate {
                    truncate_eof = Some(eof);
                }
            }

            if saw_short_page || bytes_read < read_len {
                break;
            }
            idx = run_end;
        }

        Ok((total_read, truncate_eof))
    }

    pub(super) fn read_cached_with_open(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        fh: u64,
        file_flags: u32,
    ) -> Result<usize, SystemError> {
        let md = self.cached_or_fetch_metadata()?;
        let file_size = md.size.max(0) as usize;
        if offset >= file_size || len == 0 {
            return Ok(0);
        }

        let read_len = core::cmp::min(len, file_size - offset);
        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let end_page_index = (offset + read_len - 1) >> MMArch::PAGE_SHIFT;
        let page_cache = self.ensure_page_cache()?;
        let _invalidate = page_cache.invalidate_read();
        let last_file_page = (file_size - 1) >> MMArch::PAGE_SHIFT;
        let max_pages_by_read = core::cmp::max(1, self.conn().max_read() >> MMArch::PAGE_SHIFT);
        let max_pages_by_conn = core::cmp::min(max_pages_by_read, self.conn().max_pages());
        let readaround_pages = core::cmp::min(max_pages_by_conn, 16);
        let prefetch_end = core::cmp::min(
            last_file_page + 1,
            core::cmp::max(
                end_page_index + 1,
                start_page_index.saturating_add(readaround_pages),
            ),
        );
        let (_, mut truncate_eof) = self.fill_page_cache_range_with_open(
            &page_cache,
            start_page_index,
            prefetch_end,
            file_size,
            fh,
            file_flags,
        )?;

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
                    let (eof, should_truncate) =
                        self.note_short_read_eof(page_index, read_len, file_size)?;
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

            if filled_len.is_some_and(|read_len| read_len < MMArch::PAGE_SIZE)
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
        if file_size == 0 || page_index.saturating_mul(MMArch::PAGE_SIZE) >= file_size {
            return Err(SystemError::EINVAL);
        }
        let page_cache = self.ensure_page_cache()?;
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
                let (eof, _) = self.note_short_read_eof(page_index, read_len, file_size)?;
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
    ) -> Result<usize, SystemError> {
        if req_pages == 0 {
            return Ok(0);
        }

        let md = self.cached_metadata_snapshot().ok_or(SystemError::EIO)?;
        let file_size = md.size.max(0) as usize;
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

        let (total_read, truncate_eof) = self.fill_page_cache_range_with_open(
            &page_cache,
            page_index,
            end_page,
            file_size,
            fh,
            file_flags,
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
        if let Some(md) = self.cached_metadata.lock().as_mut() {
            if end > md.size.max(0) as usize {
                md.size = end as i64;
            }
        }
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
