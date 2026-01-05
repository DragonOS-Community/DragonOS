//! To make `Ext4FuseFs` behave like `RefFS`, these FUSE interfaces
//! need to be implemented.
//!
//! init destroy lookup forget getattr setattr readlink mknod mkdir
//! unlink rmdir symlink rename link open read write flush release
//! fsync opendir readdir releasedir fsyncdir statfs setxattr getxattr
//! listxattr removexattr access create getlk ioctl
//!
//! Rust crate `fuser` doesn't have the detailed explantion of these interfaces.
//! See `fuse_lowlevel_ops` in C FUSE library for details.
//! https://libfuse.github.io/doxygen/structfuse__lowlevel__ops.html
//!
//! To support state checkpoint and restore, `Ext4FuseFs` uses a hash map
//! to store checkpoint states. By using special `ioctl` commands, `Ext4FuseFs`
//! can save and restore checkpoint states like `RefFS`, and thus support
//! Metis model check.

use super::common::{sys_time2second, time_or_now2second, translate_attr, translate_ftype};
use crate::block_dev::StateBlockDevice;
use another_ext4::{ErrCode, Ext4, Ext4Error, FileType as Ext4FileType, InodeMode, SetAttr};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::sync::Arc;
use std::time::Duration;

type FId = u64;
type StateKey = u64;

pub struct StateExt4FuseFs<T> {
    /// Block device
    block_dev: Arc<dyn StateBlockDevice<T>>,
    /// Ext4 filesystem
    fs: Ext4,
    /// Checkpoint states
    states: HashMap<StateKey, T>,
    /// Next file handler id
    next_fid: FId,
    /// Next directory handler id
    next_did: FId,
}

impl<T: 'static> StateExt4FuseFs<T> {
    const CHECKPOINT_IOC: u32 = 1;
    const RESTORE_IOC: u32 = 2;

    /// Create a file system on a block device
    /// 
    /// `init` - If true, initialize the filesystem
    pub fn new(block_dev: Arc<dyn StateBlockDevice<T>>, init: bool) -> Self {
        let mut fs = Ext4::load(block_dev.clone()).expect("Failed to load ext4 filesystem");
        if init {
            fs.init().expect("Failed to init ext4 filesystem");
        }
        Self {
            fs,
            block_dev,
            states: HashMap::new(),
            next_fid: 0,
            next_did: 0,
        }
    }

    /// Save a state
    fn checkpoint(&mut self, key: StateKey) -> bool {
        log::info!("Checkpoint {}", key);
        self.states
            .insert(key, self.block_dev.checkpoint())
            .is_none()
    }

    /// Restore a state
    fn restore(&mut self, key: StateKey) -> bool {
        log::info!("Restore {}", key);
        if let Some(state) = self.states.remove(&key) {
            self.block_dev.restore(state);
            true
        } else {
            false
        }
    }

    /// Get file attribute and tranlate type
    fn get_attr(&self, inode: u32) -> Result<FileAttr, Ext4Error> {
        match self.fs.getattr(inode) {
            Ok(attr) => Ok(translate_attr(attr)),
            Err(e) => Err(e),
        }
    }
}

impl<T: 'static> Filesystem for StateExt4FuseFs<T> {
    fn destroy(&mut self) {
        // 空实现
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        match self.fs.lookup(parent as u32, name.to_str().unwrap()) {
            Ok(inode_id) => reply.entry(&get_ttl(), &self.get_attr(inode_id).unwrap(), 0),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        match self.get_attr(ino as u32) {
            Ok(attr) => reply.attr(&get_ttl(), &attr),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn setattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<fuser::TimeOrNow>,
        mtime: Option<fuser::TimeOrNow>,
        ctime: Option<std::time::SystemTime>,
        _fh: Option<u64>,
        crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let attr = SetAttr {
            mode: mode.map(|m| InodeMode::from_bits_truncate(m as u16)),
            uid,
            gid,
            size,
            atime: atime.map(|t| time_or_now2second(t)),
            mtime: mtime.map(|t| time_or_now2second(t)),
            ctime: ctime.map(|t| sys_time2second(t)),
            crtime: crtime.map(|t| sys_time2second(t)),
        };
        match self.fs.setattr(ino as u32, attr) {
            Ok(_) => reply.attr(&get_ttl(), &self.get_attr(ino as u32).unwrap()),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn create(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        _flags: i32,
        reply: ReplyCreate,
    ) {
        // Check if name is already in use
        if let Ok(_) = self.fs.lookup(parent as u32, name.to_str().unwrap()) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        match self.fs.create(
            parent as u32,
            name.to_str().unwrap(),
            InodeMode::from_bits_truncate(mode as u16),
        ) {
            Ok(ino) => {
                reply.created(
                    &get_ttl(),
                    &self.get_attr(ino).unwrap(),
                    0,
                    self.next_fid,
                    0,
                );
                self.next_fid += 1;
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let attr = self.get_attr(ino as u32);
        match attr {
            Ok(attr) => {
                if attr.kind != FileType::RegularFile {
                    return reply.error(ErrCode::EISDIR as i32);
                }
            }
            Err(e) => return reply.error(e.code() as i32),
        }
        reply.opened(self.next_fid, 0);
        self.next_fid += 1;
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let mut data = vec![0; size as usize];
        match self.fs.read(ino as u32, offset as usize, &mut data) {
            Ok(sz) => reply.data(&data[..sz]),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        match self.fs.write(ino as u32, offset as usize, data) {
            Ok(sz) => reply.written(sz as u32),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        // Check if newname is already in use
        if let Ok(_) = self.fs.lookup(newparent as u32, newname.to_str().unwrap()) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        match self
            .fs
            .link(ino as u32, newparent as u32, newname.to_str().unwrap())
        {
            Ok(_) => reply.entry(&get_ttl(), &self.get_attr(ino as u32).unwrap(), 0),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.fs.unlink(parent as u32, name.to_str().unwrap()) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn rename(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        newparent: u64,
        newname: &OsStr,
        _flags: u32,
        reply: ReplyEmpty,
    ) {
        if parent == newparent && name == newname {
            return reply.ok();
        }
        if let Ok(src) = self.fs.lookup(parent as u32, name.to_str().unwrap()) {
            // Check if newname is already in use
            if let Ok(des) = self.fs.lookup(newparent as u32, newname.to_str().unwrap()) {
                if self.fs.getattr(src).unwrap().ftype == Ext4FileType::Directory
                    && self.fs.getattr(des).unwrap().ftype == Ext4FileType::Directory
                    && self.fs.listdir(des).unwrap().len() <= 2
                {
                    // Overwrite empty directory
                    if let Err(e) = self.fs.rmdir(newparent as u32, newname.to_str().unwrap()) {
                        return reply.error(e.code() as i32);
                    }
                } else {
                    return reply.error(ErrCode::ENOTEMPTY as i32);
                }
            }
        } else {
            return reply.error(ErrCode::ENOENT as i32);
        }
        match self.fs.rename(
            parent as u32,
            name.to_str().unwrap(),
            newparent as u32,
            newname.to_str().unwrap(),
        ) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn mkdir(
        &mut self,
        _req: &Request<'_>,
        parent: u64,
        name: &OsStr,
        mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        // Check if name is already in use
        if let Ok(_) = self.fs.lookup(parent as u32, name.to_str().unwrap()) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        match self.fs.mkdir(
            parent as u32,
            name.to_str().unwrap(),
            InodeMode::from_bits_truncate(mode as u16),
        ) {
            Ok(ino) => reply.entry(&get_ttl(), &self.get_attr(ino).unwrap(), 0),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        match self.get_attr(ino as u32) {
            Ok(attr) => {
                if attr.kind != FileType::Directory {
                    return reply.error(ErrCode::ENOTDIR as i32);
                }
                reply.opened(self.next_did, 0);
                self.next_did += 1;
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let entries = self.fs.listdir(ino as u32);
        match entries {
            Ok(entries) => {
                let mut i = offset as usize;
                while i < entries.len() {
                    let entry = &entries[i];
                    if reply.add(
                        ino,
                        i as i64 + 1,
                        translate_ftype(self.fs.getattr(entry.inode()).unwrap().ftype),
                        entry.name(),
                    ) {
                        break;
                    }
                    i += 1;
                }
                reply.ok();
            }
            Err(e) => {
                reply.error(e.code() as i32);
            }
        }
    }

    fn releasedir(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        match self.fs.rmdir(parent as u32, name.to_str().unwrap()) {
            Ok(()) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn access(&mut self, req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        let attr = self.fs.getattr(ino as u32).unwrap();
        let mask = mask as u16;
        // Check other
        if attr.perm.contains(InodeMode::from_bits_truncate(mask)) {
            return reply.ok();
        }
        // Check group
        if attr.gid == req.gid() {
            if attr.perm.contains(InodeMode::from_bits_truncate(mask << 3)) {
                return reply.ok();
            }
        }
        // Check user
        if attr.uid == req.uid() {
            if attr.perm.contains(InodeMode::from_bits_truncate(mask << 6)) {
                return reply.ok();
            }
        }
        reply.error(ErrCode::EACCES as i32);
    }

    fn ioctl(
        &mut self,
        _req: &Request<'_>,
        _ino: u64,
        _fh: u64,
        _flags: u32,
        cmd: u32,
        in_data: &[u8],
        _out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        match cmd {
            Self::CHECKPOINT_IOC => {
                let key = StateKey::from_ne_bytes(in_data[0..8].try_into().unwrap());
                if self.checkpoint(key) {
                    reply.ioctl(0, in_data);
                } else {
                    reply.error(-1);
                }
            }
            Self::RESTORE_IOC => {
                let key = StateKey::from_ne_bytes(in_data[0..8].try_into().unwrap());
                if self.restore(key) {
                    reply.ioctl(0, in_data);
                } else {
                    reply.error(-1);
                }
            }
            _ => {
                log::error!("Unknown ioctl command: {}", cmd);
                reply.error(ErrCode::ENOTSUP as i32);
            }
        }
    }

    fn getxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        size: u32,
        reply: fuser::ReplyXattr,
    ) {
        let name = name.to_str().unwrap();
        match self.fs.getxattr(ino as u32, name) {
            Ok(value) => {
                log::trace!(
                    "Get xattr {} of inode {}: {:?}",
                    name,
                    ino,
                    String::from_utf8_lossy(&value)
                );
                if size == 0 {
                    reply.size(value.len() as u32);
                } else if value.len() <= size as usize {
                    reply.data(&value);
                } else {
                    reply.error(ErrCode::ERANGE as i32);
                }
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        _flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let name = name.to_str().unwrap();
        // Check conflict
        if let Ok(_) = self.fs.getxattr(ino as u32, name) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        match self.fs.setxattr(ino as u32, name, value) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn removexattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        let name = name.to_str().unwrap();
        match self.fs.removexattr(ino as u32, name) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: fuser::ReplyXattr) {
        match self.fs.listxattr(ino as u32) {
            Ok(names) => {
                let mut buffer = Vec::new();
                for name in names {
                    buffer.extend_from_slice(name.as_bytes());
                    buffer.push(0);
                }
                if size == 0 {
                    reply.size(buffer.len() as u32);
                } else if buffer.len() <= size as usize {
                    reply.data(&buffer);
                } else {
                    reply.error(ErrCode::ERANGE as i32);
                }
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }
}

fn get_ttl() -> Duration {
    Duration::from_secs(1)
}
