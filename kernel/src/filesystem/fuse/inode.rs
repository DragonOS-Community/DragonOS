use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::mem::size_of;

use system_error::SystemError;

use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::{
        file::{FileFlags, FuseDirPrivateData, FuseFilePrivateData},
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
        fuse_pack_struct, fuse_read_struct, FuseAttr, FuseAttrOut, FuseDirent, FuseEntryOut,
        FuseGetattrIn, FuseMkdirIn, FuseMknodIn, FuseOpenIn, FuseOpenOut, FuseReadIn,
        FuseReleaseIn, FuseRenameIn, FuseSetattrIn, FuseWriteIn, FuseWriteOut, FATTR_GID,
        FATTR_MODE, FATTR_SIZE, FATTR_UID, FUSE_GETATTR, FUSE_LOOKUP, FUSE_MKDIR, FUSE_MKNOD,
        FUSE_OPEN, FUSE_OPENDIR, FUSE_READ, FUSE_READDIR, FUSE_RELEASE, FUSE_RELEASEDIR,
        FUSE_RENAME, FUSE_RMDIR, FUSE_SETATTR, FUSE_UNLINK, FUSE_WRITE,
    },
};

#[derive(Debug)]
pub struct FuseNode {
    fs: Weak<FuseFS>,
    conn: Arc<FuseConn>,
    nodeid: u64,
    parent_nodeid: Mutex<u64>,
    cached_metadata: Mutex<Option<Metadata>>,
}

impl FuseNode {
    pub fn new(
        fs: Weak<FuseFS>,
        conn: Arc<FuseConn>,
        nodeid: u64,
        parent_nodeid: u64,
        cached: Option<Metadata>,
    ) -> Arc<Self> {
        Arc::new(Self {
            fs,
            conn,
            nodeid,
            parent_nodeid: Mutex::new(parent_nodeid),
            cached_metadata: Mutex::new(cached),
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
        let file_type = if mode.contains(InodeMode::S_IFDIR) {
            FileType::Dir
        } else if mode.contains(InodeMode::S_IFREG) {
            FileType::File
        } else if mode.contains(InodeMode::S_IFLNK) {
            FileType::SymLink
        } else if mode.contains(InodeMode::S_IFCHR) {
            FileType::CharDevice
        } else if mode.contains(InodeMode::S_IFBLK) {
            FileType::BlockDevice
        } else if mode.contains(InodeMode::S_IFSOCK) {
            FileType::Socket
        } else if mode.contains(InodeMode::S_IFIFO) {
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
        *self.cached_metadata.lock() = Some(md.clone());
        Ok(md)
    }

    fn cached_or_fetch_metadata(&self) -> Result<Metadata, SystemError> {
        self.conn.check_allow_current_process()?;
        if let Some(m) = self.cached_metadata.lock().clone() {
            return Ok(m);
        }
        self.fetch_attr()
    }

    fn open_common(
        &self,
        opcode: u32,
        data: &mut FilePrivateData,
        flags: &FileFlags,
    ) -> Result<(), SystemError> {
        let open_in = FuseOpenIn {
            flags: flags.bits(),
            open_flags: 0,
        };
        let payload = self
            .conn()
            .request(opcode, self.nodeid, fuse_pack_struct(&open_in))?;
        let out: FuseOpenOut = fuse_read_struct(&payload)?;

        let conn_any: Arc<dyn core::any::Any + Send + Sync> = self.conn.clone();
        match opcode {
            FUSE_OPEN => {
                *data = FilePrivateData::FuseFile(FuseFilePrivateData {
                    conn: conn_any,
                    fh: out.fh,
                    open_flags: open_in.flags,
                });
            }
            FUSE_OPENDIR => {
                *data = FilePrivateData::FuseDir(FuseDirPrivateData {
                    conn: conn_any,
                    fh: out.fh,
                    open_flags: open_in.flags,
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
            FilePrivateData::FuseFile(p) => self.release_common(FUSE_RELEASE, p.fh, p.open_flags),
            FilePrivateData::FuseDir(p) => self.release_common(FUSE_RELEASEDIR, p.fh, p.open_flags),
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
        self.ensure_regular()?;
        if buf.len() < len {
            return Err(SystemError::EINVAL);
        }
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
        let write_in = FuseWriteIn {
            fh: p.fh,
            offset: offset as u64,
            size: len as u32,
            write_flags: 0,
            lock_owner: 0,
            flags: 0,
            padding: 0,
        };
        let mut payload_in = Vec::with_capacity(size_of::<FuseWriteIn>() + len);
        payload_in.extend_from_slice(fuse_pack_struct(&write_in));
        payload_in.extend_from_slice(&buf[..len]);
        let payload = self.conn().request(FUSE_WRITE, self.nodeid, &payload_in)?;
        let out: FuseWriteOut = fuse_read_struct(&payload)?;
        Ok(out.size as usize)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.cached_or_fetch_metadata()
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        // Minimal setattr: mode/uid/gid/size
        let mut valid = 0u32;
        valid |= FATTR_MODE;
        valid |= FATTR_UID;
        valid |= FATTR_GID;
        valid |= FATTR_SIZE;

        let inarg = FuseSetattrIn {
            valid,
            padding: 0,
            fh: 0,
            size: metadata.size as u64,
            lock_owner: 0,
            atime: 0,
            mtime: 0,
            ctime: 0,
            atimensec: 0,
            mtimensec: 0,
            ctimensec: 0,
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
        *self.cached_metadata.lock() = Some(md);
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
        *self.cached_metadata.lock() = Some(md);
        Ok(())
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
            let payload =
                self.conn()
                    .request(FUSE_READDIR, self.nodeid, fuse_pack_struct(&read_in))?;
            if payload.is_empty() {
                break;
            }

            let mut pos: usize = 0;
            let mut last_off: u64 = offset;
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

            if last_off == offset {
                // Avoid infinite loop if userspace doesn't advance offsets.
                break;
            }
            offset = last_off;
        }

        // RELEASEDIR (best-effort)
        let _ = self.release_common(FUSE_RELEASEDIR, fh, open_flags);
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
                let md = Self::attr_to_metadata(&entry.attr);
                let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
                Ok(fs.get_or_create_node(entry.nodeid, self.nodeid, Some(md)))
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
                let md = Self::attr_to_metadata(&entry.attr);
                let fs = self.fs.upgrade().ok_or(SystemError::ENOENT)?;
                Ok(fs.get_or_create_node(entry.nodeid, self.nodeid, Some(md)))
            }
            _ => Err(SystemError::ENOSYS),
        }
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
        _flag: RenameFlags,
    ) -> Result<(), SystemError> {
        self.ensure_dir()?;
        let target_any = target
            .as_any_ref()
            .downcast_ref::<FuseNode>()
            .ok_or(SystemError::EXDEV)?;

        let inarg = FuseRenameIn {
            newdir: target_any.nodeid,
        };
        let mut payload_in = Vec::new();
        payload_in.extend_from_slice(fuse_pack_struct(&inarg));
        payload_in.extend_from_slice(old_name.as_bytes());
        payload_in.push(0);
        payload_in.extend_from_slice(new_name.as_bytes());
        payload_in.push(0);
        let _ = self.conn().request(FUSE_RENAME, self.nodeid, &payload_in)?;
        Ok(())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(format!("fuse:{}", self.nodeid))
    }
}
