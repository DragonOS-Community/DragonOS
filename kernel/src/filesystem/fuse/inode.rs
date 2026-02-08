use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::mem::size_of;
use core::sync::atomic::{AtomicU64, Ordering};

use system_error::SystemError;

use crate::time::timekeep::ktime_get_real_ns;
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        file::{FileFlags, FuseDirPrivateData, FuseFilePrivateData},
        permission::PermissionMask,
        syscall::RenameFlags,
        FilePrivateData, FileSystem, FileType, IndexNode, InodeFlags, InodeId, InodeMode, Metadata,
    },
    libs::mutex::{Mutex, MutexGuard},
    time::PosixTimeSpec,
};

use super::{
    conn::FuseConn,
    fs::FuseFS,
    protocol::{
        fuse_pack_struct, fuse_read_struct, FuseAccessIn, FuseAttr, FuseAttrOut, FuseCreateIn,
        FuseDirent, FuseDirentPlus, FuseEntryOut, FuseFlushIn, FuseFsyncIn, FuseGetattrIn,
        FuseLinkIn, FuseMkdirIn, FuseMknodIn, FuseOpenIn, FuseOpenOut, FuseReadIn, FuseReleaseIn,
        FuseRename2In, FuseRenameIn, FuseSetattrIn, FuseWriteIn, FuseWriteOut, FATTR_ATIME,
        FATTR_CTIME, FATTR_GID, FATTR_MODE, FATTR_MTIME, FATTR_SIZE, FATTR_UID, FUSE_ACCESS,
        FUSE_CREATE, FUSE_FLUSH, FUSE_FSYNC, FUSE_FSYNCDIR, FUSE_FSYNC_FDATASYNC, FUSE_GETATTR,
        FUSE_LINK, FUSE_LOOKUP, FUSE_MKDIR, FUSE_MKNOD, FUSE_OPEN, FUSE_OPENDIR, FUSE_READ,
        FUSE_READDIR, FUSE_READDIRPLUS, FUSE_READLINK, FUSE_RELEASE, FUSE_RELEASEDIR, FUSE_RENAME,
        FUSE_RENAME2, FUSE_RMDIR, FUSE_ROOT_ID, FUSE_SETATTR, FUSE_SYMLINK, FUSE_UNLINK,
        FUSE_WRITE,
    },
};

#[derive(Debug)]
pub struct FuseNode {
    fs: Weak<FuseFS>,
    conn: Arc<FuseConn>,
    nodeid: u64,
    parent_nodeid: Mutex<u64>,
    cached_metadata: Mutex<Option<Metadata>>,
    cached_metadata_deadline_ns: AtomicU64,
    lookup_count: AtomicU64,
}

impl FuseNode {
    pub fn new(
        fs: Weak<FuseFS>,
        conn: Arc<FuseConn>,
        nodeid: u64,
        parent_nodeid: u64,
        cached: Option<Metadata>,
    ) -> Arc<Self> {
        let has_cached = cached.is_some();
        Arc::new(Self {
            fs,
            conn,
            nodeid,
            parent_nodeid: Mutex::new(parent_nodeid),
            cached_metadata: Mutex::new(cached),
            cached_metadata_deadline_ns: AtomicU64::new(if has_cached { u64::MAX } else { 0 }),
            lookup_count: AtomicU64::new(0),
        })
    }

    pub fn nodeid(&self) -> u64 {
        self.nodeid
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

    fn conn(&self) -> &Arc<FuseConn> {
        &self.conn
    }

    fn request_name(&self, opcode: u32, nodeid: u64, name: &str) -> Result<Vec<u8>, SystemError> {
        let mut payload = Vec::with_capacity(name.len() + 1);
        payload.extend_from_slice(name.as_bytes());
        payload.push(0);
        self.conn().request(opcode, nodeid, &payload)
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

    fn open_common(
        &self,
        opcode: u32,
        data: &mut FilePrivateData,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        if self.conn.should_skip_open(opcode) {
            let conn_any: Arc<dyn core::any::Any + Send + Sync> = self.conn.clone();
            match opcode {
                FUSE_OPEN => {
                    *data = FilePrivateData::FuseFile(FuseFilePrivateData {
                        conn: conn_any,
                        fh: 0,
                        open_flags: flags.bits(),
                        no_open: true,
                    });
                    return Ok(());
                }
                FUSE_OPENDIR => {
                    *data = FilePrivateData::FuseDir(FuseDirPrivateData {
                        conn: conn_any,
                        fh: 0,
                        open_flags: flags.bits(),
                        no_open: true,
                    });
                    return Ok(());
                }
                _ => return Err(SystemError::EINVAL),
            }
        }

        let open_in = FuseOpenIn {
            flags: flags.bits(),
            open_flags: 0,
        };
        let payload = match self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&open_in))
        {
            Ok(v) => v,
            Err(SystemError::ENOSYS) if self.conn.open_enosys_is_supported(opcode) => {
                self.conn.mark_no_open(opcode);
                let conn_any: Arc<dyn core::any::Any + Send + Sync> = self.conn.clone();
                match opcode {
                    FUSE_OPEN => {
                        *data = FilePrivateData::FuseFile(FuseFilePrivateData {
                            conn: conn_any,
                            fh: 0,
                            open_flags: open_in.flags,
                            no_open: true,
                        });
                        return Ok(());
                    }
                    FUSE_OPENDIR => {
                        *data = FilePrivateData::FuseDir(FuseDirPrivateData {
                            conn: conn_any,
                            fh: 0,
                            open_flags: open_in.flags,
                            no_open: true,
                        });
                        return Ok(());
                    }
                    _ => return Err(SystemError::EINVAL),
                }
            }
            Err(e) => return Err(e),
        };
        let out: FuseOpenOut = fuse_read_struct(&payload)?;

        let conn_any: Arc<dyn core::any::Any + Send + Sync> = self.conn.clone();
        match opcode {
            FUSE_OPEN => {
                *data = FilePrivateData::FuseFile(FuseFilePrivateData {
                    conn: conn_any,
                    fh: out.fh,
                    open_flags: open_in.flags,
                    no_open: false,
                });
            }
            FUSE_OPENDIR => {
                *data = FilePrivateData::FuseDir(FuseDirPrivateData {
                    conn: conn_any,
                    fh: out.fh,
                    open_flags: open_in.flags,
                    no_open: false,
                });
            }
            _ => return Err(SystemError::EINVAL),
        }
        Ok(())
    }

    fn release_common(&self, opcode: u32, fh: u64, open_flags: u32) -> Result<(), SystemError> {
        let inarg = FuseReleaseIn {
            fh,
            flags: open_flags,
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

    fn fsync_with_file_data(
        &self,
        datasync: bool,
        data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let (opcode, fh, no_open) = match data {
            FilePrivateData::FuseFile(p) => (FUSE_FSYNC, p.fh, p.no_open),
            FilePrivateData::FuseDir(p) => (FUSE_FSYNCDIR, p.fh, p.no_open),
            _ => return self.fsync_common(datasync),
        };

        // Linux 对 no_open/no_opendir 语义允许缺省 open，fh 不可靠，直接成功返回。
        if no_open {
            return Ok(());
        }

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
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let md = Self::attr_to_metadata(&entry.attr);
        let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
        let child = fs.get_or_create_node(entry.nodeid, self.nodeid, Some(md));
        child.inc_lookup(1);
        child.set_cached_metadata_with_valid(
            Self::attr_to_metadata(&entry.attr),
            entry.attr_valid,
            entry.attr_valid_nsec,
        );
        Ok(child)
    }
}

impl IndexNode for FuseNode {
    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    fn open(
        &self,
        mut data: MutexGuard<FilePrivateData>,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let md = self.cached_or_fetch_metadata()?;
        match md.file_type {
            FileType::Dir => self.open_common(FUSE_OPENDIR, &mut data, flags),
            FileType::File => self.open_common(FUSE_OPEN, &mut data, flags),
            _ => Err(SystemError::EINVAL),
        }
    }

    fn close(&self, data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        match &*data {
            FilePrivateData::FuseFile(p) => {
                if p.no_open {
                    return Ok(());
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
                self.release_common(FUSE_RELEASE, p.fh, p.open_flags)
            }
            FilePrivateData::FuseDir(p) => {
                if p.no_open {
                    Ok(())
                } else {
                    self.release_common(FUSE_RELEASEDIR, p.fh, p.open_flags)
                }
            }
            _ => Ok(()),
        }
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
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
        let FilePrivateData::FuseFile(p) = &*data else {
            return Err(SystemError::EBADF);
        };
        let read_in = FuseReadIn {
            fh: p.fh,
            offset: offset as u64,
            size: len as u32,
            read_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        let payload = self
            .conn()
            .request(FUSE_READ, self.nodeid, fuse_pack_struct(&read_in))?;
        let n = core::cmp::min(payload.len(), len);
        buf[..n].copy_from_slice(&payload[..n]);
        Ok(n)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.ensure_regular()?;
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
        let FilePrivateData::FuseFile(p) = &*data else {
            return Err(SystemError::EBADF);
        };
        let max_write = self.conn().max_write();
        let mut total_written = 0usize;

        while total_written < len {
            let chunk = core::cmp::min(max_write, len - total_written);
            let chunk_offset = offset
                .checked_add(total_written)
                .ok_or(SystemError::EOVERFLOW)?;

            let write_in = FuseWriteIn {
                fh: p.fh,
                offset: chunk_offset as u64,
                size: chunk as u32,
                write_flags: 0,
                lock_owner: 0,
                flags: 0,
                padding: 0,
            };
            let mut payload_in = Vec::with_capacity(size_of::<FuseWriteIn>() + chunk);
            payload_in.extend_from_slice(fuse_pack_struct(&write_in));
            payload_in.extend_from_slice(&buf[total_written..total_written + chunk]);
            let payload = self.conn().request(FUSE_WRITE, self.nodeid, &payload_in)?;
            let out: FuseWriteOut = fuse_read_struct(&payload)?;
            let wrote = core::cmp::min(out.size as usize, chunk);
            total_written += wrote;
            if wrote < chunk {
                break;
            }
        }

        Ok(total_written)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.cached_or_fetch_metadata()
    }

    fn check_access(&self, mask: PermissionMask) -> Result<(), SystemError> {
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
        self.set_cached_metadata_with_valid(md, out.attr_valid, out.attr_valid_nsec);
        Ok(())
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
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
        self.set_cached_metadata_with_valid(md, out.attr_valid, out.attr_valid_nsec);
        Ok(())
    }

    fn sync(&self) -> Result<(), SystemError> {
        self.fsync_common(false)
    }

    fn datasync(&self) -> Result<(), SystemError> {
        self.fsync_common(true)
    }

    fn sync_file(
        &self,
        datasync: bool,
        data: MutexGuard<FilePrivateData>,
    ) -> Result<(), SystemError> {
        self.fsync_with_file_data(datasync, &data)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        self.ensure_dir()?;

        // OPENDIR
        let mut pdata = FilePrivateData::Unused;
        let flags = FileFlags::O_RDONLY;
        self.open_common(FUSE_OPENDIR, &mut pdata, &flags)?;
        let FilePrivateData::FuseDir(dir_p) = &pdata else {
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
                flags: 0,
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

            let mut pos: usize = 0;
            let mut last_off: u64 = offset;
            if use_readdirplus {
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
                            if plus.entry_out.nodeid != 0 {
                                if let Some(fs) = self.fs.upgrade() {
                                    let md = Self::attr_to_metadata(&plus.entry_out.attr);
                                    let child = fs.get_or_create_node(
                                        plus.entry_out.nodeid,
                                        self.nodeid,
                                        Some(md),
                                    );
                                    child.inc_lookup(1);
                                    child.set_cached_metadata_with_valid(
                                        Self::attr_to_metadata(&plus.entry_out.attr),
                                        plus.entry_out.attr_valid,
                                        plus.entry_out.attr_valid_nsec,
                                    );
                                }
                            }
                        }
                    }

                    last_off = dirent.off;
                    let rec_len_unaligned = size_of::<FuseDirentPlus>() + dirent.namelen as usize;
                    let rec_len = (rec_len_unaligned + 8 - 1) & !(8 - 1);
                    if rec_len == 0 {
                        break;
                    }
                    pos = pos.saturating_add(rec_len);
                }
            } else {
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
                    let rec_len_unaligned = size_of::<FuseDirent>() + dirent.namelen as usize;
                    let rec_len = (rec_len_unaligned + 8 - 1) & !(8 - 1);
                    if rec_len == 0 {
                        break;
                    }
                    pos = pos.saturating_add(rec_len);
                }
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
        let child = fs.get_or_create_node(entry.nodeid, self.nodeid, Some(md));
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
        let mut payload_in = Vec::with_capacity(size_of::<FuseCreateIn>() + name.len() + 1);
        payload_in.extend_from_slice(fuse_pack_struct(&inarg));
        payload_in.extend_from_slice(name.as_bytes());
        payload_in.push(0);

        let payload = match self.conn().request(FUSE_CREATE, self.nodeid, &payload_in) {
            Ok(v) => v,
            Err(SystemError::ENOSYS) => return self.create_with_data(name, file_type, mode, 0),
            Err(e) => return Err(e),
        };
        let (entry, _) = Self::parse_create_reply(&payload)?;
        self.create_node_from_entry(&entry)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: InodeMode,
        _data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_dir()?;

        match file_type {
            FileType::Dir => {
                let inarg = FuseMkdirIn {
                    mode: (InodeMode::S_IFDIR | mode).bits(),
                    umask: 0,
                };
                let mut payload_in = Vec::with_capacity(size_of::<FuseMkdirIn>() + name.len() + 1);
                payload_in.extend_from_slice(fuse_pack_struct(&inarg));
                payload_in.extend_from_slice(name.as_bytes());
                payload_in.push(0);
                let payload = self.conn().request(FUSE_MKDIR, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry)
            }
            FileType::File => {
                let inarg = FuseMknodIn {
                    mode: (InodeMode::S_IFREG | mode).bits(),
                    rdev: 0,
                    umask: 0,
                    padding: 0,
                };
                let mut payload_in = Vec::with_capacity(size_of::<FuseMknodIn>() + name.len() + 1);
                payload_in.extend_from_slice(fuse_pack_struct(&inarg));
                payload_in.extend_from_slice(name.as_bytes());
                payload_in.push(0);
                let payload = self.conn().request(FUSE_MKNOD, self.nodeid, &payload_in)?;
                let entry: FuseEntryOut = fuse_read_struct(&payload)?;
                self.create_node_from_entry(&entry)
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
                self.create_node_from_entry(&entry)
            }
            _ => Err(SystemError::ENOSYS),
        }
    }

    fn symlink(&self, name: &str, target: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.ensure_dir()?;
        let mut payload_in = Vec::with_capacity(target.len() + name.len() + 2);
        payload_in.extend_from_slice(target.as_bytes());
        payload_in.push(0);
        payload_in.extend_from_slice(name.as_bytes());
        payload_in.push(0);
        let payload = self
            .conn()
            .request(FUSE_SYMLINK, self.nodeid, &payload_in)?;
        let entry: FuseEntryOut = fuse_read_struct(&payload)?;
        self.create_node_from_entry(&entry)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.ensure_dir()?;
        let target = other
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;
        let inarg = FuseLinkIn {
            oldnodeid: target.nodeid,
        };
        let mut payload_in = Vec::with_capacity(size_of::<FuseLinkIn>() + name.len() + 1);
        payload_in.extend_from_slice(fuse_pack_struct(&inarg));
        payload_in.extend_from_slice(name.as_bytes());
        payload_in.push(0);
        let payload = self.conn().request(FUSE_LINK, self.nodeid, &payload_in)?;
        let _entry: FuseEntryOut = fuse_read_struct(&payload)?;
        Ok(())
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.ensure_dir()?;
        let _ = self.request_name(FUSE_UNLINK, self.nodeid, name)?;
        Ok(())
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
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
        let r = self.conn().request(opcode, self.nodeid, &payload_in);
        if opcode == FUSE_RENAME2 && matches!(r, Err(SystemError::ENOSYS)) {
            return Err(SystemError::EINVAL);
        }
        let _ = r?;
        Ok(())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(format!("fuse:{}", self.nodeid))
    }
}

impl Drop for FuseNode {
    fn drop(&mut self) {
        self.flush_forget();
    }
}
