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
            file::FileFlags, permission::PermissionMask, syscall::RenameFlags, utils::DName,
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
    conn::FuseConn,
    fs::FuseFS,
    private_data::{FuseFilePrivateData, FuseOpenPrivateData, FuseWritebackHandle},
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAccessIn, FuseAttr, FuseAttrOut, FuseCreateIn,
        FuseDirent, FuseDirentPlus, FuseEntryOut, FuseFlushIn, FuseFsyncIn, FuseGetattrIn,
        FuseLinkIn, FuseMkdirIn, FuseMknodIn, FuseOpenIn, FuseOpenOut, FuseReadIn, FuseReleaseIn,
        FuseRename2In, FuseRenameIn, FuseSetattrIn, FuseWriteIn, FuseWriteOut, FATTR_ATIME,
        FATTR_CTIME, FATTR_GID, FATTR_MODE, FATTR_MTIME, FATTR_SIZE, FATTR_UID, FOPEN_DIRECT_IO,
        FOPEN_KEEP_CACHE, FUSE_ACCESS, FUSE_CREATE, FUSE_FLUSH, FUSE_FSYNC, FUSE_FSYNCDIR,
        FUSE_FSYNC_FDATASYNC, FUSE_GETATTR, FUSE_LINK, FUSE_LOOKUP, FUSE_MKDIR, FUSE_MKNOD,
        FUSE_OPEN, FUSE_OPENDIR, FUSE_READ, FUSE_READDIR, FUSE_READDIRPLUS, FUSE_READLINK,
        FUSE_RELEASE, FUSE_RELEASEDIR, FUSE_RENAME, FUSE_RENAME2, FUSE_RMDIR, FUSE_ROOT_ID,
        FUSE_SETATTR, FUSE_SYMLINK, FUSE_UNLINK, FUSE_WRITE, FUSE_WRITE_CACHE,
    },
};

#[derive(Debug)]
pub struct FuseNode {
    fs: Weak<FuseFS>,
    conn: Arc<FuseConn>,
    self_ref: Weak<FuseNode>,
    nodeid: u64,
    parent_nodeid: Mutex<u64>,
    name: Mutex<Option<String>>,
    cached_metadata: Mutex<Option<Metadata>>,
    page_cache: Mutex<Option<Arc<PageCache>>>,
    writeback_handles: Mutex<Vec<Arc<FuseWritebackHandle>>>,
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
        cached: Option<Metadata>,
    ) -> Arc<Self> {
        let has_cached = cached.is_some();
        Arc::new_cyclic(|self_ref| Self {
            fs,
            conn,
            self_ref: self_ref.clone(),
            nodeid,
            parent_nodeid: Mutex::new(parent_nodeid),
            name: Mutex::new(None),
            cached_metadata: Mutex::new(cached),
            page_cache: Mutex::new(None),
            writeback_handles: Mutex::new(Vec::new()),
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

    fn truncate_page_cache(&self, new_size: usize) -> Result<(), SystemError> {
        if let Some(cache) = self.cached_page_cache() {
            cache.truncate(new_size)?;
        }
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

    fn fuse_file_snapshot(data: &FilePrivateData) -> Result<(u64, u32, u32), SystemError> {
        match data {
            FilePrivateData::Fuse(FuseFilePrivateData::File(p)) => {
                Ok((p.fh, p.open_flags, p.fopen_flags))
            }
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
    ) -> Option<Arc<FuseWritebackHandle>> {
        if !Self::open_flags_are_writable(open_flags) {
            return None;
        }
        let handle = Arc::new(FuseWritebackHandle::new(
            fh,
            open_flags,
            fopen_flags,
            no_open,
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
        let max_write = self.conn().max_write();
        if max_write == 0 {
            return Err(SystemError::EIO);
        }

        let base_offset = page_index
            .checked_mul(MMArch::PAGE_SIZE)
            .ok_or(SystemError::EOVERFLOW)?;
        let mut total = 0usize;
        while total < buf.len() {
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
            let payload = self.conn().request(FUSE_WRITE, self.nodeid, &payload_in)?;
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
        let writeback_handle = if opcode == FUSE_OPEN {
            self.register_writeback_handle(open_flags, fh, fopen_flags, no_open)
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
                writeback_handle,
            })),
            FUSE_OPENDIR => FilePrivateData::Fuse(FuseFilePrivateData::Dir(FuseOpenPrivateData {
                conn: conn_any,
                node,
                fh,
                open_flags,
                fopen_flags,
                no_open,
                writeback_handle: None,
            })),
            _ => return Err(SystemError::EINVAL),
        };
        Ok(())
    }

    fn align_dirent_record_len(base_len: usize) -> usize {
        (base_len + Self::FUSE_DIRENT_ALIGN - 1) & !(Self::FUSE_DIRENT_ALIGN - 1)
    }

    fn cache_child_from_entry(&self, entry: &FuseEntryOut, name: &str) {
        if entry.nodeid == 0 {
            return;
        }
        if let Some(fs) = self.fs.upgrade() {
            let md = Self::attr_to_metadata(&entry.attr);
            let child = fs.get_or_create_node_with_generation(
                entry.nodeid,
                self.nodeid,
                Some(md.clone()),
                Some(entry.generation),
            );
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
        let ifmt = mode.bits() & InodeMode::S_IFMT.bits();
        let file_type = if ifmt == InodeMode::S_IFDIR.bits() {
            FileType::Dir
        } else if ifmt == InodeMode::S_IFREG.bits() {
            FileType::File
        } else if ifmt == InodeMode::S_IFLNK.bits() {
            FileType::SymLink
        } else if ifmt == InodeMode::S_IFCHR.bits() {
            FileType::CharDevice
        } else if ifmt == InodeMode::S_IFBLK.bits() {
            FileType::BlockDevice
        } else if ifmt == InodeMode::S_IFSOCK.bits() {
            FileType::Socket
        } else if ifmt == InodeMode::S_IFIFO.bits() {
            FileType::Pipe
        } else {
            FileType::File
        };

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

    fn release_common(&self, opcode: u32, fh: u64, file_flags: u32) -> Result<(), SystemError> {
        let inarg = FuseReleaseIn {
            fh,
            flags: file_flags,
            release_flags: 0,
            lock_owner: 0,
        };
        let _ = self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&inarg))?;
        Ok(())
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
        let inarg = FuseFsyncIn {
            fh: 0,
            fsync_flags: if datasync { FUSE_FSYNC_FDATASYNC } else { 0 },
            padding: 0,
        };
        let _ = self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&inarg))?;
        Ok(())
    }

    fn fsync_with_fh(&self, opcode: u32, fh: u64, datasync: bool) -> Result<(), SystemError> {
        let inarg = FuseFsyncIn {
            fh,
            fsync_flags: if datasync { FUSE_FSYNC_FDATASYNC } else { 0 },
            padding: 0,
        };
        let _ = self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&inarg))?;
        Ok(())
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
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        let md = Self::attr_to_metadata(&entry.attr);
        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        let child = fs.get_or_create_node_with_generation(
            entry.nodeid,
            self.nodeid,
            Some(md),
            Some(entry.generation),
        );
        if let Some(name) = name {
            child.set_dname(name);
        }
        child.inc_lookup(1);
        child.set_cached_metadata_with_valid(
            Self::attr_to_metadata(&entry.attr),
            entry.attr_valid,
            entry.attr_valid_nsec,
        );
        Ok(child)
    }

    fn read_direct_with_open(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        fh: u64,
        file_flags: u32,
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
                read_flags: 0,
                lock_owner: 0,
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
        offset: usize,
        data: &[u8],
    ) -> Result<(), SystemError> {
        if data.is_empty() {
            return Ok(());
        }
        let Some(page_cache) = self.cached_page_cache() else {
            return Ok(());
        };
        let end = offset
            .checked_add(data.len())
            .ok_or(SystemError::EOVERFLOW)?;
        let _invalidate = page_cache.invalidate_read();

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
            let _ = page_cache.manager().update_clean_page(
                page_index,
                page_offset,
                &data[src_offset..src_offset + write_len],
            )?;
        }

        Ok(())
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
            self.invalidate_clean_page_cache()?;
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

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // `IndexNode::close()` is called from `File::drop()`, i.e. after the
        // last `Arc<File>` reference is gone.  FUSE file/dir private data is
        // immutable after open, so taking a snapshot here is not a TOCTOU
        // window; it prevents holding the private-data mutex while waiting for
        // userspace to reply to FLUSH/RELEASE.
        let fuse_data = match &*data {
            FilePrivateData::Fuse(data) => data.clone(),
            _ => return Ok(()),
        };
        drop(data);

        match fuse_data {
            FuseFilePrivateData::File(p) => {
                let writeback_result = if p.writeback_handle.is_some() {
                    self.sync_cached_pages()
                } else {
                    Ok(())
                };
                if let Some(handle) = &p.writeback_handle {
                    self.unregister_writeback_handle(handle);
                }
                if p.no_open {
                    return writeback_result;
                }
                let flush_in = FuseFlushIn {
                    fh: p.fh,
                    unused: 0,
                    padding: 0,
                    lock_owner: 0,
                };
                let _ = self
                    .conn()
                    .request(FUSE_FLUSH, self.nodeid, fuse_pack_struct(&flush_in));
                let release_result = self.release_common(FUSE_RELEASE, p.fh, p.open_flags);
                writeback_result.and(release_result)
            }
            FuseFilePrivateData::Dir(p) => {
                if p.no_open {
                    Ok(())
                } else {
                    self.release_common(FUSE_RELEASEDIR, p.fh, p.open_flags)
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
        let (fh, file_flags, fopen_flags) = Self::fuse_file_snapshot(&data)?;
        drop(data);

        if (fopen_flags & FOPEN_DIRECT_IO) != 0 || (file_flags & FileFlags::O_DIRECT.bits()) != 0 {
            return self.read_direct_with_open(offset, len, buf, fh, file_flags);
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
        let (fh, file_flags, fopen_flags) = Self::fuse_file_snapshot(&data)?;
        drop(data);
        let max_write = self.conn().max_write();
        if max_write == 0 {
            return Err(SystemError::EIO);
        }
        let cached_write =
            (fopen_flags & FOPEN_DIRECT_IO) == 0 && (file_flags & FileFlags::O_DIRECT.bits()) == 0;
        let mut total_written = 0usize;

        while total_written < len {
            let chunk = core::cmp::min(max_write, len - total_written);
            let chunk_offset = offset
                .checked_add(total_written)
                .ok_or(SystemError::EOVERFLOW)?;

            let write_in = FuseWriteIn {
                fh,
                offset: chunk_offset as u64,
                size: chunk as u32,
                write_flags: 0,
                lock_owner: 0,
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
                self.update_cached_pages_after_write(
                    chunk_offset,
                    &buf[total_written..total_written + wrote],
                )
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
        self.check_not_stale()?;
        let inarg = FuseSetattrIn {
            valid: FATTR_SIZE,
            padding: 0,
            fh: 0,
            size: len as u64,
            lock_owner: 0,
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
            FuseFilePrivateData::File(p) => self.fsync_with_fh(FUSE_FSYNC, p.fh, datasync),
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
            FuseFilePrivateData::File(p) => self.fsync_with_fh(FUSE_FSYNC, p.fh, datasync),
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
            let _ = self.release_common(FUSE_RELEASEDIR, fh, open_flags);
        }
        Ok(names)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        if name == "." {
            let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
            return Ok(fs.get_or_create_node(self.nodeid, *self.parent_nodeid.lock(), None));
        }
        if name == ".." {
            return self.parent();
        }

        let payload = self.request_name(FUSE_LOOKUP, self.nodeid, name)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&entry.attr);

        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        let child = fs.get_or_create_node_with_generation(
            entry.nodeid,
            self.nodeid,
            Some(md),
            Some(entry.generation),
        );
        child.set_dname(name);
        child
            .lookup_attr_flags
            .store(entry.attr.flags, Ordering::Relaxed);
        child.inc_lookup(1);
        child.set_cached_metadata_with_valid(
            Self::attr_to_metadata(&entry.attr),
            entry.attr_valid,
            entry.attr_valid_nsec,
        );
        Ok(child)
    }

    fn parent(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        let parent_nodeid = *self.parent_nodeid.lock();
        if parent_nodeid == self.nodeid {
            return Ok(fs.root_node());
        }
        Ok(fs.get_or_create_node(parent_nodeid, parent_nodeid, None))
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
        let (entry, _) = Self::parse_create_reply(&payload)?;
        self.create_node_from_entry(&entry, Some(name))
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
                self.create_node_from_entry(&entry, Some(name))
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
                self.create_node_from_entry(&entry, Some(name))
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
                self.create_node_from_entry(&entry, Some(name))
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
        self.create_node_from_entry(&entry, Some(name))
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.check_not_stale()?;
        self.ensure_dir()?;
        let target = other
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;
        let inarg = FuseLinkIn {
            oldnodeid: target.nodeid,
        };
        let payload_in = Self::pack_struct_and_name_payload(&inarg, name);
        let payload = self.conn().request(FUSE_LINK, self.nodeid, &payload_in)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        let md = Self::attr_to_metadata(&entry.attr);
        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        let child = fs.get_or_create_node_for_link(
            entry.nodeid,
            self.nodeid,
            Some(md),
            Some(entry.generation),
        );
        child.inc_lookup(1);
        child.set_cached_metadata_with_valid(
            Self::attr_to_metadata(&entry.attr),
            entry.attr_valid,
            entry.attr_valid_nsec,
        );
        Ok(())
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
            node.set_dname(new_name);
        }
        if let Some(node) = cached_new {
            if flag.contains(RenameFlags::EXCHANGE) {
                node.set_parent_nodeid(self.nodeid);
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
        self.flush_forget();
    }
}
