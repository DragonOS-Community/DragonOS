use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use system_error::SystemError;

use crate::time::timekeep::ktime_get_real_ns;
use crate::{
    arch::MMArch,
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        page_cache::{PageCache, PageCacheBackend},
        vfs::{
            file::{FileFlags, FileMode},
            permission::PermissionMask,
            syscall::RenameFlags,
            utils::DName,
            FilePrivateData, FileSystem, FileType, IndexNode, InodeFlags, InodeId, InodeMode,
            Metadata,
        },
    },
    libs::{
        casting::DowncastArc,
        mutex::{Mutex, MutexGuard},
    },
    mm::MemoryManagementArch,
    time::PosixTimeSpec,
};

use super::{
    conn::{FuseConn, FuseRequestCred},
    fs::FuseFS,
    private_data::{
        FuseFilePrivateData, FuseOpenContext, FuseOpenPrivateData, FuseWritebackHandle,
    },
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAccessIn, FuseAttr, FuseAttrOut, FuseCreateIn,
        FuseDirent, FuseDirentPlus, FuseEntryOut, FuseFallocateIn, FuseFlushIn, FuseFsyncIn,
        FuseGetattrIn, FuseLinkIn, FuseMkdirIn, FuseMknodIn, FuseOpenIn, FuseOpenOut, FuseReadIn,
        FuseReleaseIn, FuseRename2In, FuseRenameIn, FuseSetattrIn, FuseWriteIn, FuseWriteOut,
        FATTR_ATIME, FATTR_CTIME, FATTR_FH, FATTR_GID, FATTR_LOCKOWNER, FATTR_MODE, FATTR_MTIME,
        FATTR_SIZE, FATTR_UID, FOPEN_DIRECT_IO, FOPEN_KEEP_CACHE, FOPEN_NOFLUSH, FOPEN_NONSEEKABLE,
        FOPEN_STREAM, FUSE_ACCESS, FUSE_CREATE, FUSE_FALLOCATE, FUSE_FLUSH, FUSE_FSYNC,
        FUSE_FSYNCDIR, FUSE_FSYNC_FDATASYNC, FUSE_GETATTR, FUSE_LINK, FUSE_LOOKUP, FUSE_MKDIR,
        FUSE_MKNOD, FUSE_OPEN, FUSE_OPENDIR, FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS,
        FUSE_READLINK, FUSE_READ_LOCKOWNER, FUSE_RELEASE, FUSE_RELEASEDIR, FUSE_RENAME,
        FUSE_RENAME2, FUSE_RMDIR, FUSE_ROOT_ID, FUSE_SETATTR, FUSE_SYMLINK, FUSE_UNLINK,
        FUSE_WRITE, FUSE_WRITE_CACHE, FUSE_WRITE_LOCKOWNER,
    },
};

#[derive(Debug)]
pub struct FuseNode {
    fs: Weak<FuseFS>,
    conn: Arc<FuseConn>,
    self_ref: Weak<FuseNode>,
    nodeid: u64,
    parent_nodeid: Mutex<u64>,
    parent: Mutex<Option<Arc<FuseNode>>>,
    name: Mutex<Option<String>>,
    cached_metadata: Mutex<Option<Metadata>>,
    page_cache: Mutex<Option<Arc<PageCache>>>,
    writeback_handles: Mutex<Vec<Arc<FuseWritebackHandle>>>,
    direct_io_lock: Mutex<()>,
    cached_metadata_deadline_ns: AtomicU64,
    lookup_count: AtomicU64,
    /// 最近一次 LOOKUP 回复中的 fuse_attr.flags（用于 announce-submounts）。
    lookup_attr_flags: AtomicU32,
    /// LOOKUP 返回的 generation，用于检测 virtiofsd 复用 nodeid。
    generation: AtomicU64,
    stale: AtomicBool,
}

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
    const FUSE_DIRENT_ALIGN: usize = 8;

    pub fn new(
        fs: Weak<FuseFS>,
        conn: Arc<FuseConn>,
        nodeid: u64,
        parent_nodeid: u64,
        parent: Option<Arc<FuseNode>>,
        cached: Option<Metadata>,
    ) -> Arc<Self> {
        let has_cached = cached.is_some();
        Arc::new_cyclic(|self_ref| Self {
            fs,
            conn,
            self_ref: self_ref.clone(),
            nodeid,
            parent_nodeid: Mutex::new(parent_nodeid),
            parent: Mutex::new(parent),
            name: Mutex::new(None),
            cached_metadata: Mutex::new(cached),
            page_cache: Mutex::new(None),
            writeback_handles: Mutex::new(Vec::new()),
            direct_io_lock: Mutex::new(()),
            cached_metadata_deadline_ns: AtomicU64::new(if has_cached { u64::MAX } else { 0 }),
            lookup_count: AtomicU64::new(0),
            lookup_attr_flags: AtomicU32::new(0),
            generation: AtomicU64::new(0),
            stale: AtomicBool::new(false),
        })
    }

    pub fn lookup_attr_flags(&self) -> u32 {
        self.lookup_attr_flags.load(Ordering::Relaxed)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    pub(crate) fn set_generation(&self, gen: u64) {
        self.generation.store(gen, Ordering::Relaxed);
    }

    pub(crate) fn mark_stale(&self) {
        self.stale.store(true, Ordering::Release);
    }

    fn check_not_stale(&self) -> Result<(), SystemError> {
        if self.stale.load(Ordering::Acquire) {
            return Err(SystemError::ESTALE);
        }
        Ok(())
    }

    pub fn nodeid(&self) -> u64 {
        self.nodeid
    }

    pub(crate) fn set_dname(&self, name: &str) {
        *self.name.lock() = Some(name.to_string());
    }

    pub(crate) fn has_dname(&self, name: &str) -> bool {
        self.name.lock().as_deref() == Some(name)
    }

    pub(crate) fn clear_dname_if(&self, name: &str) {
        let mut dname = self.name.lock();
        if dname.as_deref() == Some(name) {
            *dname = None;
        }
    }

    pub fn set_parent_nodeid(&self, parent: u64) {
        *self.parent_nodeid.lock() = parent;
    }

    pub(crate) fn set_parent_if_absent(&self, parent: Option<Arc<FuseNode>>) {
        let Some(parent) = parent else {
            return;
        };
        if parent.nodeid() == self.nodeid {
            return;
        }
        let mut guard = self.parent.lock();
        if guard.is_none() {
            *guard = Some(parent);
        }
    }

    pub(crate) fn set_parent(&self, parent: Option<Arc<FuseNode>>) {
        if parent
            .as_ref()
            .is_some_and(|parent| parent.nodeid() == self.nodeid)
        {
            return;
        }
        *self.parent.lock() = parent;
    }

    pub(crate) fn clear_parent(&self) {
        *self.parent.lock() = None;
    }

    pub fn set_cached_metadata(&self, md: Metadata) {
        *self.cached_metadata.lock() = Some(md);
        self.cached_metadata_deadline_ns
            .store(u64::MAX, Ordering::Relaxed);
    }

    pub fn set_cached_metadata_with_valid(&self, md: Metadata, valid: u64, valid_nsec: u32) {
        *self.cached_metadata.lock() = Some(md);
        self.cached_metadata_deadline_ns
            .store(Self::cache_deadline(valid, valid_nsec), Ordering::Relaxed);
    }

    /// 累计该 inode 在 userspace daemon 侧持有的 LOOKUP 引用。
    ///
    /// 对齐 Linux：每个成功的 LOOKUP/READDIRPLUS entry 都必须被记账，并在 inode
    /// 释放或卸载时用对应的 `FUSE_FORGET(nlookup=...)` 归还。打开的文件句柄会在
    /// `FuseOpenPrivateData` 中持有 `Arc<FuseNode>`，避免 fd 存活期间过早 FORGET。
    pub fn inc_lookup(&self, count: u64) {
        if self.nodeid == FUSE_ROOT_ID || count == 0 {
            return;
        }
        self.lookup_count.fetch_add(count, Ordering::Relaxed);
    }

    pub fn flush_forget(&self) {
        if self.nodeid == FUSE_ROOT_ID {
            return;
        }
        let nlookup = self.lookup_count.swap(0, Ordering::Relaxed);
        if nlookup == 0 {
            return;
        }
        let _ = self.conn.queue_forget(self.nodeid, nlookup);
    }

    fn now_ns() -> u64 {
        ktime_get_real_ns().max(0) as u64
    }

    fn cache_deadline(valid: u64, valid_nsec: u32) -> u64 {
        if valid == 0 && valid_nsec == 0 {
            return 0;
        }
        let delta_ns = valid
            .saturating_mul(1_000_000_000)
            .saturating_add(valid_nsec as u64);
        Self::now_ns().saturating_add(delta_ns)
    }

    pub(crate) fn conn(&self) -> &Arc<FuseConn> {
        &self.conn
    }

    fn ensure_page_cache(&self) -> Result<Arc<PageCache>, SystemError> {
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

    fn cached_page_cache(&self) -> Option<Arc<PageCache>> {
        self.page_cache.lock().clone()
    }

    fn cached_metadata_snapshot(&self) -> Option<Metadata> {
        self.cached_metadata.lock().clone()
    }

    fn invalidate_clean_page_cache(&self) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.unmap_mapping_pages(0, None)?;
            let _ = cache.manager().invalidate_all_clean();
        }
        Ok(())
    }

    fn discard_clean_page_cache(&self) {
        if let Some(cache) = self.cached_page_cache() {
            let _ = cache.manager().invalidate_all_clean();
        }
    }

    fn truncate_page_cache(&self, new_size: usize) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.truncate(new_size)?;
        }
        Ok(())
    }

    fn setattr_size(
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

    pub(crate) fn fuse_fs(&self) -> Option<Arc<FuseFS>> {
        self.fs.upgrade()
    }

    pub(crate) fn parent_fuse_nodeid(&self) -> u64 {
        *self.parent_nodeid.lock()
    }

    fn request_name(&self, opcode: u32, nodeid: u64, name: &str) -> Result<Vec<u8>, SystemError> {
        self.check_not_stale()?;
        let payload = Self::pack_name_payload(name);
        self.conn().request(opcode, nodeid, &payload)
    }

    fn pack_name_payload(name: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(name.len() + 1);
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload
    }

    fn pack_struct_and_name_payload<T: Copy>(inarg: &T, name: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(size_of::<T>() + name.len() + 1);
        payload.extend_from_slice(fuse_pack_struct(inarg));
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        payload
    }

    fn pack_two_names_payload(first: &str, second: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(first.len() + second.len() + 2);
        payload.extend_from_slice(first.as_bytes());
        payload.push(0);
        payload.extend_from_slice(second.as_bytes());
        payload.push(0);
        payload
    }

    /// Linux `fuse_send_open()` forwards file flags except creation-only bits.
    fn fuse_open_in_flags(&self, raw: u32) -> u32 {
        let mut flags = raw
            & !(FileFlags::O_CREAT.bits() | FileFlags::O_EXCL.bits() | FileFlags::O_NOCTTY.bits());
        if !self
            .conn
            .has_init_flag(super::protocol::FUSE_ATOMIC_O_TRUNC)
        {
            flags &= !FileFlags::O_TRUNC.bits();
        }
        flags
    }

    fn fuse_file_private_snapshot(
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

    fn unregister_writeback_handle(&self, handle: &Arc<FuseWritebackHandle>) {
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

    fn sync_dirty_cached_pages(&self) -> Result<(), SystemError> {
        let Some(page_cache) = self.cached_page_cache() else {
            return Ok(());
        };
        if !page_cache.has_dirty_pages() {
            return Ok(());
        }
        page_cache.manager().sync()
    }

    fn check_and_advance_open_wb_error(
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

    fn flush_open_file(
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
    ) -> Result<super::private_data::FuseWritebackHandlePin, SystemError> {
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
        let max_write = self.conn().max_write();
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

    fn align_dirent_record_len(base_len: usize) -> usize {
        (base_len + Self::FUSE_DIRENT_ALIGN - 1) & !(Self::FUSE_DIRENT_ALIGN - 1)
    }

    fn entry_file_type(attr: &FuseAttr) -> Result<FileType, SystemError> {
        let mode = InodeMode::from_bits_truncate(attr.mode);
        match mode & InodeMode::S_IFMT {
            t if t == InodeMode::S_IFDIR => Ok(FileType::Dir),
            t if t == InodeMode::S_IFREG => Ok(FileType::File),
            t if t == InodeMode::S_IFLNK => Ok(FileType::SymLink),
            t if t == InodeMode::S_IFCHR => Ok(FileType::CharDevice),
            t if t == InodeMode::S_IFBLK => Ok(FileType::BlockDevice),
            t if t == InodeMode::S_IFSOCK => Ok(FileType::Socket),
            t if t == InodeMode::S_IFIFO => Ok(FileType::Pipe),
            _ => Err(SystemError::EIO),
        }
    }

    fn metadata_from_valid_entry(
        entry: &FuseEntryOut,
        zero_nodeid_error: SystemError,
        expected_type: Option<FileType>,
    ) -> Result<Metadata, SystemError> {
        if entry.nodeid == 0 {
            return Err(zero_nodeid_error);
        }
        let file_type = Self::entry_file_type(&entry.attr)?;
        if expected_type.is_some_and(|expected| expected != file_type) {
            return Err(SystemError::EIO);
        }
        Ok(Self::attr_to_metadata(&entry.attr))
    }

    fn cache_child_from_entry(&self, entry: &FuseEntryOut, name: &str) {
        let Ok(md) = Self::metadata_from_valid_entry(entry, SystemError::EIO, None) else {
            if entry.nodeid != 0 {
                let _ = self.conn.queue_forget(entry.nodeid, 1);
            }
            return;
        };
        if let (Some(fs), Some(parent)) = (self.fs.upgrade(), self.self_ref.upgrade()) {
            let Ok(child) = fs.get_or_create_node_with_generation(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
            ) else {
                let _ = self.conn.queue_forget(entry.nodeid, 1);
                return;
            };
            child.set_dname(name);
            child.inc_lookup(1);
            child.set_cached_metadata_with_valid(md, entry.attr_valid, entry.attr_valid_nsec);
        }
    }

    fn parse_readdirplus_payload(
        &self,
        payload: &[u8],
        names: &mut Vec<String>,
        mut last_off: u64,
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
                    self.cache_child_from_entry(&plus.entry_out, name);
                }
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

    fn parse_readdir_payload(
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

    fn attr_to_metadata(attr: &FuseAttr) -> Metadata {
        let mode = InodeMode::from_bits_truncate(attr.mode);
        let file_type = Self::entry_file_type(attr).unwrap_or(FileType::File);

        let inode_id = InodeId::new(attr.ino as usize);

        Metadata {
            dev_id: 0,
            inode_id,
            size: attr.size as i64,
            blk_size: attr.blksize as usize,
            blocks: attr.blocks as usize,
            atime: PosixTimeSpec::new(attr.atime as i64, attr.atimensec as i64),
            mtime: PosixTimeSpec::new(attr.mtime as i64, attr.mtimensec as i64),
            ctime: PosixTimeSpec::new(attr.ctime as i64, attr.ctimensec as i64),
            btime: PosixTimeSpec::default(),
            file_type,
            mode,
            flags: InodeFlags::empty(),
            nlinks: attr.nlink as usize,
            uid: attr.uid as usize,
            gid: attr.gid as usize,
            raw_dev: DeviceNumber::default(),
        }
    }

    fn fetch_attr(&self) -> Result<Metadata, SystemError> {
        self.check_not_stale()?;
        let getattr_in = FuseGetattrIn {
            getattr_flags: 0,
            dummy: 0,
            fh: 0,
        };
        let payload =
            self.conn()
                .request(FUSE_GETATTR, self.nodeid, fuse_pack_struct(&getattr_in))?;
        let out: FuseAttrOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&out.attr);
        self.set_cached_metadata_with_valid(md.clone(), out.attr_valid, out.attr_valid_nsec);
        Ok(md)
    }

    fn cached_or_fetch_metadata(&self) -> Result<Metadata, SystemError> {
        self.conn.check_allow_current_process()?;
        if let Some(m) = self.cached_metadata.lock().clone() {
            let deadline = self.cached_metadata_deadline_ns.load(Ordering::Relaxed);
            if deadline == u64::MAX || (deadline != 0 && Self::now_ns() < deadline) {
                return Ok(m);
            }
        }
        self.fetch_attr()
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
                .has_init_flag(super::protocol::FUSE_ATOMIC_O_TRUNC)
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

    fn open_common(
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

    fn release_common_for_node(
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

    fn release_common(&self, opcode: u32, fh: u64, file_flags: u32, lock_owner: u64) {
        self.release_common_for_node(opcode, self.nodeid, fh, file_flags, lock_owner);
    }

    fn ensure_dir(&self) -> Result<(), SystemError> {
        let md = self.cached_or_fetch_metadata()?;
        if md.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }
        Ok(())
    }

    fn ensure_regular(&self) -> Result<(), SystemError> {
        let md = self.cached_or_fetch_metadata()?;
        if md.file_type != FileType::File {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }

    fn fsync_common(&self, datasync: bool) -> Result<(), SystemError> {
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

    fn fsync_with_fh(&self, opcode: u32, fh: u64, datasync: bool) -> Result<(), SystemError> {
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

    fn parse_create_reply(payload: &[u8]) -> Result<(FuseEntryOut, FuseOpenOut), SystemError> {
        let entry_size = size_of::<FuseEntryOut>();
        let open_size = size_of::<FuseOpenOut>();
        if payload.len() < entry_size + open_size {
            return Err(SystemError::EINVAL);
        }
        let entry: FuseEntryOut = fuse_read_struct(&payload[..entry_size])?;
        let open_out: FuseOpenOut = fuse_read_struct(&payload[entry_size..entry_size + open_size])?;
        Ok((entry, open_out))
    }

    fn create_node_from_entry(
        &self,
        entry: &FuseEntryOut,
        name: Option<&str>,
        expected_type: FileType,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut consumed = false;
        let result = (|| {
            self.check_not_stale()?;
            let md = Self::metadata_from_valid_entry(entry, SystemError::EIO, Some(expected_type))?;
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let child = fs.get_or_create_node_with_generation(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
            )?;
            if let Some(name) = name {
                child.set_dname(name);
            }
            child.inc_lookup(1);
            consumed = true;
            child.set_cached_metadata_with_valid(md, entry.attr_valid, entry.attr_valid_nsec);
            Ok(child as Arc<dyn IndexNode>)
        })();
        if result.is_err() && entry.nodeid != 0 && !consumed {
            let _ = self.conn.queue_forget(entry.nodeid, 1);
        }
        result
    }

    fn read_direct_with_open(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        fh: u64,
        file_flags: u32,
        lock_owner: u64,
    ) -> Result<usize, SystemError> {
        let max_read = self.conn().max_read();
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

    fn read_cached_with_open(
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

        let mut dst_offset = 0usize;
        let mut truncate_eof = None;
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

    fn update_cached_pages_after_write(
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

    fn prepare_cached_write_range(
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

    fn note_successful_write(&self, offset: usize, len: usize) -> Result<(), SystemError> {
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

    fn invalidate_cached_pages_after_direct_write(
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

    fn prepare_direct_io_range(
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

impl IndexNode for FuseNode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn read_sync(&self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let _ = (offset, buf);
        self.check_not_stale()?;
        self.ensure_regular()?;
        Err(SystemError::ENOSYS)
    }

    fn write_sync(&self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let _ = (offset, buf);
        self.check_not_stale()?;
        self.ensure_regular()?;
        Err(SystemError::ENOSYS)
    }

    fn mmap(&self, start: usize, len: usize, offset: usize) -> Result<(), SystemError> {
        let _ = (start, len, offset);
        self.check_not_stale()?;
        self.ensure_regular()?;
        self.ensure_page_cache()?;
        Ok(())
    }

    fn check_mmap_file(
        &self,
        file: &Arc<crate::filesystem::vfs::file::File>,
        len: usize,
        offset: usize,
        vm_flags: crate::mm::VmFlags,
    ) -> Result<(), SystemError> {
        let _ = (len, offset);
        self.check_not_stale()?;
        if file.file_type() != FileType::File {
            return Err(SystemError::EINVAL);
        }

        let fopen_flags = {
            let data = file.private_data.lock();
            let FilePrivateData::Fuse(FuseFilePrivateData::File(p)) = &*data else {
                return Err(SystemError::EINVAL);
            };
            p.fopen_flags
        };

        if (fopen_flags & FOPEN_DIRECT_IO) != 0
            && vm_flags.contains(crate::mm::VmFlags::VM_MAYSHARE)
        {
            return Err(SystemError::ENODEV);
        }

        Ok(())
    }

    fn mmap_file(
        &self,
        file: &Arc<crate::filesystem::vfs::file::File>,
        start: usize,
        len: usize,
        offset: usize,
        vm_flags: crate::mm::VmFlags,
    ) -> Result<(), SystemError> {
        let _ = (start, len, offset);
        self.check_not_stale()?;
        if file.file_type() != FileType::File {
            return Err(SystemError::EINVAL);
        }

        let fopen_flags = {
            let data = file.private_data.lock();
            let FilePrivateData::Fuse(FuseFilePrivateData::File(p)) = &*data else {
                return Err(SystemError::EINVAL);
            };
            p.fopen_flags
        };

        if (fopen_flags & FOPEN_DIRECT_IO) != 0 {
            if vm_flags.contains(crate::mm::VmFlags::VM_MAYSHARE) {
                return Err(SystemError::ENODEV);
            }
            self.discard_clean_page_cache();
        }

        self.ensure_page_cache()?;
        Ok(())
    }

    fn truncate_before_open(&self, flags: &FileFlags) -> bool {
        flags.contains(FileFlags::O_TRUNC)
            && !self
                .conn
                .has_init_flag(super::protocol::FUSE_ATOMIC_O_TRUNC)
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let md = self.cached_or_fetch_metadata()?;
        match md.file_type {
            FileType::Dir => self.open_common(FUSE_OPENDIR, &mut data, flags),
            FileType::File => self.open_common(FUSE_OPEN, &mut data, flags),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn adjust_file_mode_after_open(&self, data: &FilePrivateData, mode: &mut FileMode) {
        let fopen_flags = match data {
            FilePrivateData::Fuse(FuseFilePrivateData::File(p))
            | FilePrivateData::Fuse(FuseFilePrivateData::Dir(p)) => p.fopen_flags,
            _ => return,
        };

        if (fopen_flags & FOPEN_STREAM) != 0 {
            mode.remove(
                FileMode::FMODE_LSEEK
                    | FileMode::FMODE_PREAD
                    | FileMode::FMODE_PWRITE
                    | FileMode::FMODE_ATOMIC_POS,
            );
            mode.insert(FileMode::FMODE_STREAM);
        } else if (fopen_flags & FOPEN_NONSEEKABLE) != 0 {
            mode.remove(FileMode::FMODE_LSEEK | FileMode::FMODE_PREAD | FileMode::FMODE_PWRITE);
        }
    }

    fn flush_file(
        &self,
        data: MutexGuard<FilePrivateData>,
        lock_owner: u64,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => return Ok(()),
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => self.flush_open_file(&p, lock_owner),
            FuseFilePrivateData::Dir(_) | FuseFilePrivateData::Dev(_) => Ok(()),
        }
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // `IndexNode::close()` is called from `File::drop()`, i.e. after the
        // last `Arc<File>` reference is gone.  User-visible FUSE_FLUSH errors
        // are handled by `flush_file()` on fd close; this final close only
        // drains dirty mappings and sends RELEASE.
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => return Ok(()),
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                let writeback_result = if p.writeback_handle.is_some() {
                    self.sync_dirty_cached_pages()
                } else {
                    Ok(())
                };
                if let Some(handle) = &p.writeback_handle {
                    self.unregister_writeback_handle(handle);
                }
                if p.no_open {
                    return writeback_result;
                }
                self.release_common(FUSE_RELEASE, p.fh, p.open_flags, 0);
                writeback_result
            }
            FuseFilePrivateData::Dir(p) => {
                if p.no_open {
                    Ok(())
                } else {
                    self.release_common(FUSE_RELEASEDIR, p.fh, p.open_flags, 0);
                    Ok(())
                }
            }
            FuseFilePrivateData::Dev(_) => Ok(()),
        }
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let md = self.cached_or_fetch_metadata()?;
        if md.file_type == FileType::SymLink {
            if offset != 0 {
                return Ok(0);
            }
            let payload = self.conn().request(FUSE_READLINK, self.nodeid, &[])?;
            let n = core::cmp::min(payload.len(), len);
            buf[..n].copy_from_slice(&payload[..n]);
            return Ok(n);
        }
        self.ensure_regular()?;
        let private_data = Self::fuse_file_private_snapshot(&data)?;
        drop(data);
        let fh = private_data.fh;
        let file_flags = private_data.open_flags;
        let fopen_flags = private_data.fopen_flags;

        if (fopen_flags & FOPEN_DIRECT_IO) != 0 || (file_flags & FileFlags::O_DIRECT.bits()) != 0 {
            self.prepare_direct_io_range(offset, len, &private_data, false)?;
            let lock_owner = crate::filesystem::vfs::vcore::current_file_lock_owner_id();
            return self.read_direct_with_open(offset, len, buf, fh, file_flags, lock_owner);
        }

        self.read_cached_with_open(offset, len, buf, fh, file_flags)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.check_not_stale()?;
        self.ensure_regular()?;
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        if len > 0 {
            offset.checked_add(len).ok_or(SystemError::EOVERFLOW)?;
        }
        let private_data = Self::fuse_file_private_snapshot(&data)?;
        drop(data);
        let fh = private_data.fh;
        let file_flags = private_data.open_flags;
        let fopen_flags = private_data.fopen_flags;
        let max_write = self.conn().max_write();
        if max_write == 0 {
            return Err(SystemError::EIO);
        }
        let cached_write =
            (fopen_flags & FOPEN_DIRECT_IO) == 0 && (file_flags & FileFlags::O_DIRECT.bits()) == 0;
        let lock_owner = if cached_write {
            0
        } else {
            crate::filesystem::vfs::vcore::current_file_lock_owner_id()
        };
        let _direct_write_guard = if cached_write {
            None
        } else {
            Some(self.direct_io_lock.lock())
        };
        let mut total_written = 0usize;
        if !cached_write {
            self.prepare_direct_io_range(offset, len, &private_data, true)?;
        }
        let cached_page_cache = if cached_write {
            self.cached_page_cache()
        } else {
            None
        };
        let _cached_write_guard = cached_page_cache
            .as_ref()
            .map(|page_cache| page_cache.invalidate_write());
        if let Some(page_cache) = cached_page_cache.as_ref() {
            // Serialize ordinary cached writes against page-cache writeback so an older
            // dirty mmap page cannot be written back after the daemon sees this write.
            self.prepare_cached_write_range(page_cache, offset, len)?;
        }

        while total_written < len {
            let chunk = core::cmp::min(max_write, len - total_written);
            let chunk_offset = offset
                .checked_add(total_written)
                .ok_or(SystemError::EOVERFLOW)?;

            let write_in = FuseWriteIn {
                fh,
                offset: chunk_offset as u64,
                size: chunk as u32,
                write_flags: if lock_owner != 0 {
                    FUSE_WRITE_LOCKOWNER
                } else {
                    0
                },
                lock_owner,
                flags: file_flags,
                padding: 0,
            };
            let mut payload_in = Vec::with_capacity(size_of::<FuseWriteIn>() + chunk);
            payload_in.extend_from_slice(fuse_pack_struct(&write_in));
            payload_in.extend_from_slice(&buf[total_written..total_written + chunk]);
            let payload = self.conn().request(FUSE_WRITE, self.nodeid, &payload_in)?;
            let out: FuseWriteOut = fuse_read_struct(&payload)?;
            if out.size as usize > chunk {
                return if total_written > 0 {
                    Ok(total_written)
                } else {
                    Err(SystemError::EIO)
                };
            }
            let wrote = out.size as usize;
            self.note_successful_write(chunk_offset, wrote)?;
            let cache_result = if cached_write {
                if let Some(page_cache) = cached_page_cache.as_ref() {
                    self.update_cached_pages_after_write(
                        page_cache,
                        chunk_offset,
                        &buf[total_written..total_written + wrote],
                    )
                } else {
                    Ok(())
                }
            } else {
                self.invalidate_cached_pages_after_direct_write(chunk_offset, wrote)
            };
            total_written += wrote;
            if cache_result.is_err() {
                break;
            }
            if wrote < chunk {
                break;
            }
        }

        Ok(total_written)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.check_not_stale()?;
        self.cached_or_fetch_metadata()
    }

    fn check_access(&self, mask: PermissionMask) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let inarg = FuseAccessIn {
            mask: mask.bits() & PermissionMask::MAY_RWX.bits(),
            padding: 0,
        };
        let _ = self
            .conn()
            .request(FUSE_ACCESS, self.nodeid, fuse_pack_struct(&inarg))?;
        Ok(())
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.check_not_stale()?;
        let old = self.cached_or_fetch_metadata()?;
        let mut valid = 0u32;
        if metadata.mode != old.mode {
            valid |= FATTR_MODE;
        }
        if metadata.uid != old.uid {
            valid |= FATTR_UID;
        }
        if metadata.gid != old.gid {
            valid |= FATTR_GID;
        }
        if metadata.size != old.size {
            valid |= FATTR_SIZE;
        }
        if metadata.atime != old.atime {
            valid |= FATTR_ATIME;
        }
        if metadata.mtime != old.mtime {
            valid |= FATTR_MTIME;
        }
        if metadata.ctime != old.ctime {
            valid |= FATTR_CTIME;
        }
        if valid == 0 {
            return Ok(());
        }

        let inarg = FuseSetattrIn {
            valid,
            padding: 0,
            fh: 0,
            size: metadata.size as u64,
            lock_owner: 0,
            atime: metadata.atime.tv_sec as u64,
            mtime: metadata.mtime.tv_sec as u64,
            ctime: metadata.ctime.tv_sec as u64,
            atimensec: metadata.atime.tv_nsec as u32,
            mtimensec: metadata.mtime.tv_nsec as u32,
            ctimensec: metadata.ctime.tv_nsec as u32,
            mode: metadata.mode.bits(),
            unused4: 0,
            uid: metadata.uid as u32,
            gid: metadata.gid as u32,
            unused5: 0,
        };
        let payload = self
            .conn()
            .request(FUSE_SETATTR, self.nodeid, fuse_pack_struct(&inarg))?;
        let out: FuseAttrOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&out.attr);
        let new_size = md.size.max(0) as usize;
        self.set_cached_metadata_with_valid(md, out.attr_valid, out.attr_valid_nsec);
        if (valid & FATTR_SIZE) != 0 {
            self.truncate_page_cache(new_size)?;
        }
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.setattr_size(len, None, None)
    }

    fn resize_with_lock_owner(&self, len: usize, lock_owner: u64) -> Result<(), SystemError> {
        self.setattr_size(len, Some(lock_owner), None)
    }

    fn resize_file(
        &self,
        len: usize,
        lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.resize_with_lock_owner(len, lock_owner);
            }
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => self.setattr_size(len, Some(lock_owner), Some(p.fh)),
            _ => self.resize_with_lock_owner(len, lock_owner),
        }
    }

    fn fallocate_file(
        &self,
        mode: i32,
        offset: usize,
        len: usize,
        _lock_owner: u64,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        if mode != 0 {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }
        self.check_not_stale()?;
        let new_size = offset.checked_add(len).ok_or(SystemError::EFBIG)?;
        if self.conn().no_fallocate() {
            return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
        }

        let fuse_data = match &*data {
            FilePrivateData::Fuse(FuseFilePrivateData::File(data)) => data.clone(),
            _ => return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP),
        };
        drop(data);

        let md = self.metadata()?;
        if new_size > md.size.max(0) as usize {
            crate::filesystem::vfs::vcore::check_file_size_limit(new_size)?;
        }

        let in_arg = FuseFallocateIn {
            fh: fuse_data.fh,
            offset: offset as u64,
            length: len as u64,
            mode: mode as u32,
            padding: 0,
        };
        match self
            .conn()
            .request(FUSE_FALLOCATE, self.nodeid, fuse_pack_struct(&in_arg))
        {
            Ok(_) => {
                if let Some(md) = self.cached_metadata.lock().as_mut() {
                    if new_size > md.size.max(0) as usize {
                        md.size = new_size as i64;
                    }
                }
                self.cached_metadata_deadline_ns.store(0, Ordering::Relaxed);
                Ok(())
            }
            Err(SystemError::ENOSYS) => {
                self.conn().mark_no_fallocate();
                Err(SystemError::EOPNOTSUPP_OR_ENOTSUP)
            }
            Err(e) => Err(e),
        }
    }

    fn sync(&self) -> Result<(), SystemError> {
        self.fsync_common(false)
    }

    fn datasync(&self) -> Result<(), SystemError> {
        self.fsync_common(true)
    }

    fn page_cache(&self) -> Option<Arc<PageCache>> {
        self.cached_page_cache()
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.fsync_common(datasync);
            }
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                let sync_result = self.sync_cached_pages();
                let wb_error_result = self.check_and_advance_open_wb_error(&p);
                sync_result?;
                wb_error_result?;
                self.fsync_with_fh(FUSE_FSYNC, p.fh, datasync)
            }
            FuseFilePrivateData::Dir(p) => self.fsync_with_fh(FUSE_FSYNCDIR, p.fh, datasync),
            FuseFilePrivateData::Dev(_) => self.fsync_common(datasync),
        }
    }

    fn sync_file_range(
        &self,
        start: usize,
        end: usize,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => {
                drop(data);
                return self.fsync_common(datasync);
            }
        };
        drop(data);

        if let FuseFilePrivateData::File(_) = &fuse_data {
            if let Some(page_cache) = self.cached_page_cache() {
                let start_index = start >> MMArch::PAGE_SHIFT;
                let end_index = end >> MMArch::PAGE_SHIFT;
                page_cache
                    .manager()
                    .writeback_range(start_index, end_index)?;
            }
        }

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                self.check_and_advance_open_wb_error(&p)?;
                self.fsync_with_fh(FUSE_FSYNC, p.fh, datasync)
            }
            FuseFilePrivateData::Dir(p) => self.fsync_with_fh(FUSE_FSYNCDIR, p.fh, datasync),
            FuseFilePrivateData::Dev(_) => self.fsync_common(datasync),
        }
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;

        // OPENDIR
        let mut pdata = FilePrivateData::Unused;
        let flags = FileFlags::O_RDONLY;
        self.open_common(FUSE_OPENDIR, &mut pdata, &flags)?;
        let FilePrivateData::Fuse(FuseFilePrivateData::Dir(dir_p)) = &pdata else {
            return Err(SystemError::EINVAL);
        };
        let fh = dir_p.fh;
        let open_flags = dir_p.open_flags;
        let mut use_readdirplus = self.conn.use_readdirplus();

        let mut names: Vec<String> = Vec::new();
        let mut offset: u64 = 0;

        loop {
            let read_in = FuseReadIn {
                fh,
                offset,
                size: 64 * 1024,
                read_flags: 0,
                lock_owner: 0,
                flags: open_flags,
                padding: 0,
            };
            let opcode = if use_readdirplus {
                FUSE_READDIRPLUS
            } else {
                FUSE_READDIR
            };
            let payload = match self
                .conn()
                .request(opcode, self.nodeid, fuse_pack_struct(&read_in))
            {
                Ok(v) => v,
                Err(SystemError::ENOSYS) if use_readdirplus => {
                    self.conn.disable_readdirplus();
                    use_readdirplus = false;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if payload.is_empty() {
                break;
            }

            let mut last_off: u64 = offset;
            if use_readdirplus {
                last_off = self.parse_readdirplus_payload(&payload, &mut names, last_off)?;
            } else {
                last_off = Self::parse_readdir_payload(&payload, &mut names, last_off)?;
            }

            if last_off == offset {
                // Avoid infinite loop if userspace doesn't advance offsets.
                break;
            }
            offset = last_off;
        }

        // RELEASEDIR (best-effort)
        if !dir_p.no_open {
            self.release_common(FUSE_RELEASEDIR, fh, open_flags, 0);
        }
        Ok(names)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        if name == "." {
            let this = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            return Ok(this);
        }
        if name == ".." {
            return self.parent();
        }

        let payload = self.request_name(FUSE_LOOKUP, self.nodeid, name)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        let md = Self::metadata_from_valid_entry(&entry, SystemError::ENOENT, None).inspect_err(
            |_| {
                if entry.nodeid != 0 {
                    let _ = self.conn.queue_forget(entry.nodeid, 1);
                }
            },
        )?;

        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
        let child = fs
            .get_or_create_node_with_generation(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
            )
            .inspect_err(|_| {
                let _ = self.conn.queue_forget(entry.nodeid, 1);
            })?;
        child.set_dname(name);
        child
            .lookup_attr_flags
            .store(entry.attr.flags, Ordering::Relaxed);
        child.inc_lookup(1);
        child.set_cached_metadata_with_valid(md, entry.attr_valid, entry.attr_valid_nsec);
        Ok(child)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        if let Some(parent) = self.parent.lock().clone() {
            return Ok(parent);
        }
        let parent_nodeid = *self.parent_nodeid.lock();
        if parent_nodeid == self.nodeid {
            return Ok(fs.root_node());
        }
        Err(SystemError::ESTALE)
    }

    fn create(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        if file_type != FileType::File {
            return self.create_with_data(name, file_type, mode, 0);
        }

        let inarg = FuseCreateIn {
            flags: FileFlags::O_RDONLY.bits(),
            mode: (InodeMode::S_IFREG | mode).bits(),
            umask: 0,
            open_flags: 0,
        };
        let payload_in = Self::pack_struct_and_name_payload(&inarg, name);

        let payload = match self.conn().request(FUSE_CREATE, self.nodeid, &payload_in) {
            Ok(v) => v,
            Err(SystemError::ENOSYS) => return self.create_with_data(name, file_type, mode, 0),
            Err(e) => return Err(e),
        };
        let (entry, open_out) = Self::parse_create_reply(&payload)?;
        if entry.nodeid != 0 {
            self.release_common_for_node(
                FUSE_RELEASE,
                entry.nodeid,
                open_out.fh,
                FileFlags::O_RDONLY.bits(),
                0,
            );
        }
        self.create_node_from_entry(&entry, Some(name), FileType::File)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;

        match file_type {
            FileType::Dir => {
                let inarg = FuseMkdirIn {
                    mode: (InodeMode::S_IFDIR | mode).bits(),
                    umask: 0,
                };
                let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
                let payload = self.conn().request(FUSE_MKDIR, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry, Some(name), FileType::Dir)
            }
            FileType::File => {
                let inarg = FuseMknodIn {
                    mode: (InodeMode::S_IFREG | mode).bits(),
                    rdev: 0,
                    umask: 0,
                    padding: 0,
                };
                let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
                let payload = self.conn().request(FUSE_MKNOD, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry, Some(name), FileType::File)
            }
            FileType::SymLink => {
                let mut payload_in = Vec::with_capacity(name.len() + 2);
                payload_in.push(0);
                payload_in.extend_from_slice(name.as_bytes());
                payload_in.push(0);
                let payload = self
                    .conn()
                    .request(FUSE_SYMLINK, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry, Some(name), FileType::SymLink)
            }
            _ => Err(SystemError::ENOSYS),
        }
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let payload_in = Self::pack_two_names_payload(target, name);
        let payload = self
            .conn()
            .request(FUSE_SYMLINK, self.nodeid, &payload_in)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        self.create_node_from_entry(&entry, Some(name), FileType::SymLink)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let target = other
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;
        let expected_type = target.cached_or_fetch_metadata()?.file_type;
        let inarg = FuseLinkIn {
            oldnodeid: target.nodeid,
        };
        let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
        let payload = self.conn().request(FUSE_LINK, self.nodeid, &payload_in)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        let mut consumed = false;
        let result = (|| {
            let md =
                Self::metadata_from_valid_entry(&entry, SystemError::EIO, Some(expected_type))?;
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            let parent = self.self_ref.upgrade().ok_or(SystemError::ENOENT)?;
            let child = fs.get_or_create_node_for_link(
                entry.nodeid,
                Some(parent),
                Some(md.clone()),
                Some(entry.generation),
            )?;
            child.inc_lookup(1);
            consumed = true;
            child.set_cached_metadata_with_valid(md, entry.attr_valid, entry.attr_valid_nsec);
            Ok(())
        })();
        if result.is_err() && entry.nodeid != 0 && !consumed {
            let _ = self.conn.queue_forget(entry.nodeid, 1);
        }
        result
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let _ = self.request_name(FUSE_UNLINK, self.nodeid, name)?;
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let _ = self.request_name(FUSE_RMDIR, self.nodeid, name)?;
        Ok(())
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
        flag: RenameFlags,
    ) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let target_any = target
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;

        let mut payload_in = Vec::new();
        let opcode = if flag.is_empty() {
            let inarg = FuseRenameIn {
                newdir: target_any.nodeid,
            };
            payload_in.extend_from_slice(fuse_pack_struct(&inarg));
            FUSE_RENAME
        } else {
            let inarg = FuseRename2In {
                newdir: target_any.nodeid,
                flags: flag.bits(),
                padding: 0,
            };
            payload_in.extend_from_slice(fuse_pack_struct(&inarg));
            FUSE_RENAME2
        };
        payload_in.extend_from_slice(old_name.as_bytes());
        payload_in.push(0);
        payload_in.extend_from_slice(new_name.as_bytes());
        payload_in.push(0);
        let cached_old = self
            .fs
            .upgrade()
            .and_then(|fs| fs.find_cached_child(self.nodeid, old_name))
            .or_else(|| {
                self.find(old_name)
                    .ok()
                    .and_then(|inode| inode.downcast_arc::<FuseNode>())
            });
        let cached_new = target_any
            .fs
            .upgrade()
            .and_then(|fs| fs.find_cached_child(target_any.nodeid, new_name))
            .or_else(|| {
                if flag.contains(RenameFlags::EXCHANGE) {
                    target_any
                        .find(new_name)
                        .ok()
                        .and_then(|inode| inode.downcast_arc::<FuseNode>())
                } else {
                    None
                }
            });
        let r = self.conn().request(opcode, self.nodeid, &payload_in);
        if opcode == FUSE_RENAME2 && matches!(r, Err(SystemError::ENOSYS)) {
            return Err(SystemError::EINVAL);
        }
        let _ = r?;
        if let Some(node) = cached_old {
            node.set_parent_nodeid(target_any.nodeid);
            node.set_parent(Some(
                target_any.self_ref.upgrade().ok_or(SystemError::ENOENT)?,
            ));
            node.set_dname(new_name);
        }
        if let Some(node) = cached_new {
            if flag.contains(RenameFlags::EXCHANGE) {
                node.set_parent_nodeid(self.nodeid);
                node.set_parent(Some(self.self_ref.upgrade().ok_or(SystemError::ENOENT)?));
                node.set_dname(old_name);
            } else {
                node.clear_dname_if(new_name);
            }
        }
        Ok(())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(format!("fuse:{}", self.nodeid))
    }

    fn dname(&self) -> Result<DName, SystemError> {
        self.name
            .lock()
            .as_ref()
            .map(|name| DName(Arc::new(name.clone())))
            .ok_or(SystemError::ENOENT)
    }
}

impl Drop for FuseNode {
    fn drop(&mut self) {
        self.clear_parent();
    }
}
