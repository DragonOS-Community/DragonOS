mod directory;
mod file;
mod vfs;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use system_error::SystemError;

use crate::time::timekeep::ktime_get_real_ns;
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        page_cache::PageCache,
        vfs::{FileType, InodeFlags, InodeId, InodeMode, Metadata},
    },
    libs::mutex::Mutex,
    time::PosixTimeSpec,
};

use super::reply::FuseReply;
use super::{
    conn::FuseConn,
    fs::FuseFS,
    private_data::FuseWritebackHandle,
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAttr, FuseAttrOut, FuseEntryOut, FuseGetattrIn,
        FUSE_GETATTR, FUSE_ROOT_ID,
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
    lookup_cache: Mutex<BTreeMap<String, FuseLookupCacheEntry>>,
    direct_io_lock: Mutex<()>,
    cached_metadata_deadline_ns: AtomicU64,
    attr_version: AtomicU64,
    lookup_count: AtomicU64,
    /// 最近一次 LOOKUP 回复中的 fuse_attr.flags（用于 announce-submounts）。
    lookup_attr_flags: AtomicU32,
    /// LOOKUP 返回的 generation，用于检测 virtiofsd 复用 nodeid。
    generation: AtomicU64,
    stale: AtomicBool,
}

#[derive(Debug, Clone)]
struct FuseLookupCacheEntry {
    child: Arc<FuseNode>,
    generation: u64,
    deadline_ns: u64,
}

impl FuseNode {
    const FUSE_DIRENT_ALIGN: usize = 8;
    const LOOKUP_CACHE_MAX_ENTRIES: usize = 1024;
    const READDIR_BUFFER_SIZE: usize = 64 * 1024;
    const XATTR_SIZE_MAX: usize = 65536;
    const XATTR_LIST_MAX: usize = 65536;

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
            lookup_cache: Mutex::new(BTreeMap::new()),
            direct_io_lock: Mutex::new(()),
            cached_metadata_deadline_ns: AtomicU64::new(if has_cached { u64::MAX } else { 0 }),
            attr_version: AtomicU64::new(1),
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

    pub(crate) fn check_not_stale(&self) -> Result<(), SystemError> {
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

    pub(crate) fn cached_file_type(&self) -> Option<FileType> {
        self.cached_metadata.lock().as_ref().map(|md| md.file_type)
    }

    pub fn set_cached_metadata(&self, md: Metadata) {
        let mut metadata = self.cached_metadata.lock();
        *metadata = Some(md);
        self.attr_version.fetch_add(1, Ordering::AcqRel);
        drop(metadata);
        self.cached_metadata_deadline_ns
            .store(u64::MAX, Ordering::Relaxed);
    }

    pub fn set_cached_metadata_with_valid(&self, md: Metadata, valid: u64, valid_nsec: u32) {
        let mut metadata = self.cached_metadata.lock();
        *metadata = Some(md);
        self.attr_version.fetch_add(1, Ordering::AcqRel);
        drop(metadata);
        self.cached_metadata_deadline_ns
            .store(Self::cache_deadline(valid, valid_nsec), Ordering::Relaxed);
    }

    pub(crate) fn attr_version(&self) -> u64 {
        self.attr_version.load(Ordering::Acquire)
    }

    pub(crate) fn bump_attr_version(&self) {
        self.attr_version.fetch_add(1, Ordering::AcqRel);
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

    pub(crate) fn fuse_fs(&self) -> Option<Arc<FuseFS>> {
        self.fs.upgrade()
    }

    pub(crate) fn parent_fuse_nodeid(&self) -> u64 {
        *self.parent_nodeid.lock()
    }

    fn request_name(&self, opcode: u32, nodeid: u64, name: &str) -> Result<FuseReply, SystemError> {
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

    fn fuse_xattr_unsupported(&self, opcode: u32) -> SystemError {
        self.conn.mark_no_xattr(opcode);
        SystemError::EOPNOTSUPP_OR_ENOTSUP
    }

    fn verify_xattr_list(list: &[u8]) -> Result<(), SystemError> {
        let mut idx = 0usize;
        while idx < list.len() {
            let Some(end) = list[idx..].iter().position(|b| *b == 0) else {
                return Err(SystemError::EIO);
            };
            if end == 0 {
                return Err(SystemError::EIO);
            }
            idx += end + 1;
        }
        Ok(())
    }

    fn pack_two_names_payload(first: &str, second: &str) -> Vec<u8> {
        let mut payload = Vec::with_capacity(first.len() + second.len() + 2);
        payload.extend_from_slice(first.as_bytes());
        payload.push(0);
        payload.extend_from_slice(second.as_bytes());
        payload.push(0);
        payload
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
        if entry.attr.size > i64::MAX as u64 {
            return Err(SystemError::EIO);
        }
        let file_type = Self::entry_file_type(&entry.attr)?;
        if expected_type.is_some_and(|expected| expected != file_type) {
            return Err(SystemError::EIO);
        }
        Ok(Self::attr_to_metadata(&entry.attr))
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
}

impl Drop for FuseNode {
    fn drop(&mut self) {
        self.clear_lookup_cache_tree();
        self.flush_forget();
        self.clear_parent();
    }
}
