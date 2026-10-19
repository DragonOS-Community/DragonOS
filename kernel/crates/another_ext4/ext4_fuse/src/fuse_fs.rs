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
use another_ext4::{ErrCode, Ext4, Ext4Error, InodeMode, InodeReclaimHandle, SetAttr};
use fuser::{
    FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::sync::Arc;
use std::time::Duration;

type FId = u64;
type StateKey = u64;

// Node identities belong to one restored timeline. The root is the stable
// control anchor; all other nodes become stale on a successful restore.
#[derive(Default)]
struct NodeEpoch(u32);

impl NodeEpoch {
    fn encode(&self, inode: u32) -> u64 {
        if inode == 1 {
            1
        } else {
            (u64::from(self.0) << 32) | u64::from(inode)
        }
    }

    fn decode(&self, node: u64) -> Result<u32, i32> {
        if node == 1 {
            return Ok(1);
        }
        let inode = node as u32;
        if inode <= 1 || node >> 32 != u64::from(self.0) {
            Err(libc::ESTALE)
        } else {
            Ok(inode)
        }
    }
}

#[derive(Clone, Copy)]
struct Handle {
    node: u64,
    epoch: u32,
}

// Keep the FUSE reply/error handoff uniform at callback boundaries.
macro_rules! fuse_result {
    ($reply:ident, $value:expr) => {
        match $value {
            Ok(value) => value,
            Err(error) => return $reply.error(error),
        }
    };
}

pub struct StateExt4FuseFs<T> {
    /// Block device
    block_dev: Arc<dyn StateBlockDevice<T>>,
    /// Ext4 filesystem
    fs: Ext4,
    nodes: NodeEpoch,
    /// Checkpoint states
    states: HashMap<StateKey, T>,
    /// Next file handler id
    next_fid: FId,
    /// Next directory handler id
    next_did: FId,
    lookup_counts: HashMap<u32, u64>,
    open_handles: HashMap<FId, Handle>,
    open_dir_handles: HashMap<FId, Handle>,
    pending_reclaims: HashMap<u32, InodeReclaimHandle>,
}

impl<T: 'static> StateExt4FuseFs<T> {
    const CHECKPOINT_IOC: u32 = 1;
    const RESTORE_IOC: u32 = 2;
    // Linux _IOW(0xe4, nr, u64): the direction and size let the kernel
    // safely import the key. Legacy unencoded 1/2 cannot import a pointer.
    const CHECKPOINT_IOW: u32 = 0x4008_e401;
    const RESTORE_IOW: u32 = 0x4008_e402;

    fn snapshot_key(cmd: u32, data: &[u8]) -> Result<(bool, StateKey), i32> {
        let restore = match cmd {
            Self::CHECKPOINT_IOC | Self::CHECKPOINT_IOW => false,
            Self::RESTORE_IOC | Self::RESTORE_IOW => true,
            _ => return Err(libc::ENOTTY),
        };
        let key = data
            .try_into()
            .map(StateKey::from_ne_bytes)
            .map_err(|_| libc::EINVAL)?;
        Ok((restore, key))
    }

    /// Create a file system on a block device
    ///
    /// `init` - If true, initialize the filesystem
    pub fn new(block_dev: Arc<dyn StateBlockDevice<T>>, init: bool) -> Self {
        let mut fs = Ext4::load_writable(block_dev.clone())
            .expect("Failed to load writable ext4 filesystem");
        if init {
            fs.init().expect("Failed to init ext4 filesystem");
        }
        Self {
            fs,
            block_dev,
            nodes: NodeEpoch::default(),
            states: HashMap::new(),
            next_fid: 0,
            next_did: 0,
            lookup_counts: HashMap::new(),
            open_handles: HashMap::new(),
            open_dir_handles: HashMap::new(),
            pending_reclaims: HashMap::new(),
        }
    }

    /// Snapshot only committed synchronous filesystem operations. This tool
    /// does not enable the asynchronous batching backend.
    fn checkpoint(&mut self, key: StateKey) -> Result<(), i32> {
        if self.states.contains_key(&key) {
            return Err(libc::EEXIST);
        }
        if !self.pending_reclaims.is_empty() {
            return Err(libc::EBUSY);
        }
        self.fs.flush_device().map_err(|e| e.code() as i32)?;
        self.states.insert(key, self.block_dev.checkpoint());
        Ok(())
    }

    fn restore(&mut self, key: StateKey) -> Result<(), i32> {
        let target = self.states.get(&key).ok_or(libc::ENOENT)?;
        let epoch = self.nodes.0.checked_add(1).ok_or(libc::EOVERFLOW)?;
        let rollback = self.block_dev.checkpoint();
        self.block_dev.restore(target);
        let restored = match Ext4::load_writable(self.block_dev.clone()) {
            Ok(fs) => fs,
            Err(error) => {
                self.block_dev.restore(&rollback);
                return Err(error.code() as i32);
            }
        };
        // Ext4 Drop does not perform I/O. Never shut down the discarded owner:
        // its old cached metadata must not be written into the restored image.
        self.fs = restored;
        self.nodes.0 = epoch;
        self.states.remove(&key);
        self.lookup_counts.clear();
        self.pending_reclaims.clear();
        Ok(())
    }

    fn get_attr(&self, inode: u32) -> Result<FileAttr, Ext4Error> {
        let mut attr = translate_attr(self.fs.getattr(inode)?);
        attr.ino = self.nodes.encode(inode);
        Ok(attr)
    }

    fn allocate_handle(&mut self, directory: bool) -> Result<FId, i32> {
        let next = if directory {
            &mut self.next_did
        } else {
            &mut self.next_fid
        };
        let id = *next;
        *next = next.checked_add(1).ok_or(libc::EOVERFLOW)?;
        Ok(id)
    }

    fn handle(&self, node: u64) -> Handle {
        Handle {
            node,
            epoch: self.nodes.0,
        }
    }

    fn check_handle(&self, handles: &HashMap<FId, Handle>, fh: FId, node: u64) -> Result<u32, i32> {
        let inode = self.nodes.decode(node)?;
        let handle = handles.get(&fh).ok_or(libc::ESTALE)?;
        if handle.node != node || handle.epoch != self.nodes.0 {
            return Err(libc::ESTALE);
        }
        Ok(inode)
    }

    fn release_handle(&mut self, fh: FId, node: u64, directory: bool) -> Result<(), i32> {
        let handles = if directory {
            &mut self.open_dir_handles
        } else {
            &mut self.open_handles
        };
        let handle = handles.get(&fh).ok_or(libc::ESTALE)?;
        if handle.node != node {
            return Err(libc::ESTALE);
        }
        let handle = handles.remove(&fh).unwrap();
        if handle.epoch != self.nodes.0 {
            return Ok(());
        }
        let inode = self.nodes.decode(node)?;
        self.try_reclaim(inode).map_err(|e| e.code() as i32)
    }

    fn retain_lookup(&mut self, inode: u32) {
        *self.lookup_counts.entry(inode).or_insert(0) += 1;
    }

    fn forget_node(&mut self, node: u64, nlookup: u64) -> Result<(), Ext4Error> {
        let Ok(inode) = self.nodes.decode(node) else {
            return Ok(());
        };
        let count = self.lookup_counts.entry(inode).or_insert(0);
        *count = count.saturating_sub(nlookup);
        if *count == 0 {
            self.lookup_counts.remove(&inode);
        }
        self.try_reclaim(inode)
    }

    fn queue_reclaim(&mut self, handle: InodeReclaimHandle) -> Result<(), Ext4Error> {
        let inode = handle.inode_id();
        match self.pending_reclaims.entry(inode) {
            std::collections::hash_map::Entry::Occupied(_) => {
                return Err(Ext4Error::new(ErrCode::EINVAL));
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(handle);
            }
        }
        self.try_reclaim(inode)
    }

    fn try_reclaim(&mut self, inode: u32) -> Result<(), Ext4Error> {
        let looked_up = self.lookup_counts.get(&inode).copied().unwrap_or(0) != 0;
        let opened = self
            .open_handles
            .values()
            .chain(self.open_dir_handles.values())
            .any(|handle| {
                handle.epoch == self.nodes.0 && self.nodes.decode(handle.node) == Ok(inode)
            });
        if !looked_up && !opened {
            if let Some(handle) = self.pending_reclaims.remove(&inode) {
                if let Err(failure) = self.fs.reclaim_inode(handle) {
                    let (error, handle) = failure.into_parts();
                    self.pending_reclaims.insert(inode, handle);
                    return Err(error);
                }
            }
        }
        Ok(())
    }
}

impl<T: 'static> Filesystem for StateExt4FuseFs<T> {
    fn destroy(&mut self) {
        self.lookup_counts.clear();
        self.open_handles.clear();
        self.open_dir_handles.clear();
        let pending = std::mem::take(&mut self.pending_reclaims);
        for (_, handle) in pending {
            if let Err(error) = self.fs.reclaim_inode(handle) {
                log::error!(
                    "failed to drain ext4 reclaim during FUSE destroy: {:?}",
                    error
                );
                // No-journal final unlinks need not be on the durable orphan
                // list. Preserve the dirty mount marker when reclaim fails.
                return;
            }
        }
        if let Err(error) = self.fs.shutdown_writable() {
            log::error!("failed to cleanly shut down ext4 filesystem: {:?}", error);
        }
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let parent_inode = fuse_result!(reply, self.nodes.decode(parent));
        match self.fs.lookup(parent_inode, name.to_str().unwrap()) {
            Ok(inode_id) => {
                self.retain_lookup(inode_id);
                reply.entry(&get_ttl(), &self.get_attr(inode_id).unwrap(), 0)
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn forget(&mut self, _req: &Request<'_>, ino: u64, nlookup: u64) {
        if let Err(error) = self.forget_node(ino, nlookup) {
            log::error!("failed to reclaim forgotten ext4 node {}: {:?}", ino, error);
        }
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, reply: ReplyAttr) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        match self.get_attr(inode) {
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
        fh: Option<u64>,
        crtime: Option<std::time::SystemTime>,
        _chgtime: Option<std::time::SystemTime>,
        _bkuptime: Option<std::time::SystemTime>,
        _flags: Option<u32>,
        reply: ReplyAttr,
    ) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        if let Some(fh) = fh {
            fuse_result!(reply, self.check_handle(&self.open_handles, fh, ino));
        }
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
        match self.fs.setattr(inode, attr) {
            Ok(_) => reply.attr(&get_ttl(), &self.get_attr(inode).unwrap()),
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
        let parent_inode = fuse_result!(reply, self.nodes.decode(parent));
        // Check if name is already in use
        if let Ok(_) = self.fs.lookup(parent_inode, name.to_str().unwrap()) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        let fh = fuse_result!(reply, self.allocate_handle(false));
        match self.fs.create(
            parent_inode,
            name.to_str().unwrap(),
            InodeMode::from_bits_truncate(mode as u16),
        ) {
            Ok(ino) => {
                self.retain_lookup(ino);
                self.open_handles
                    .insert(fh, self.handle(self.nodes.encode(ino)));
                reply.created(
                    &get_ttl(),
                    &self.get_attr(ino).unwrap(),
                    0,
                    fh,
                    fuser::consts::FOPEN_DIRECT_IO,
                );
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let attr = self.get_attr(inode);
        match attr {
            Ok(attr) => {
                if attr.kind != FileType::RegularFile {
                    return reply.error(ErrCode::EISDIR as i32);
                }
            }
            Err(e) => return reply.error(e.code() as i32),
        }
        let fh = fuse_result!(reply, self.allocate_handle(false));
        self.open_handles.insert(fh, self.handle(ino));
        reply.opened(fh, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn read(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyData,
    ) {
        let inode = fuse_result!(reply, self.check_handle(&self.open_handles, fh, ino));
        let mut data = vec![0; size as usize];
        match self.fs.read(inode, offset as usize, &mut data) {
            Ok(sz) => reply.data(&data[..sz]),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn write(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock_owner: Option<u64>,
        reply: ReplyWrite,
    ) {
        let inode = fuse_result!(reply, self.check_handle(&self.open_handles, fh, ino));
        match self.fs.write(inode, offset as usize, data) {
            Ok(sz) => reply.written(sz as u32),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn release(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        _flags: i32,
        _lock_owner: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        match self.release_handle(fh, ino, false) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn link(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        newparent: u64,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let newparent_inode = fuse_result!(reply, self.nodes.decode(newparent));
        // Check if newname is already in use
        if let Ok(_) = self.fs.lookup(newparent_inode, newname.to_str().unwrap()) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        match self
            .fs
            .link(inode, newparent_inode, newname.to_str().unwrap())
        {
            Ok(_) => {
                self.retain_lookup(inode);
                reply.entry(&get_ttl(), &self.get_attr(inode).unwrap(), 0)
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_inode = fuse_result!(reply, self.nodes.decode(parent));
        match self.fs.unlink(parent_inode, name.to_str().unwrap()) {
            Ok(Some(handle)) => match self.queue_reclaim(handle) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e.code() as i32),
            },
            Ok(None) => reply.ok(),
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
        let parent_inode = fuse_result!(reply, self.nodes.decode(parent));
        let newparent_inode = fuse_result!(reply, self.nodes.decode(newparent));
        if parent == newparent && name == newname {
            return reply.ok();
        }
        match self.fs.rename(
            parent_inode,
            name.to_str().unwrap(),
            newparent_inode,
            newname.to_str().unwrap(),
        ) {
            Ok(Some(handle)) => match self.queue_reclaim(handle) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e.code() as i32),
            },
            Ok(None) => reply.ok(),
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
        let parent_inode = fuse_result!(reply, self.nodes.decode(parent));
        // Check if name is already in use
        if let Ok(_) = self.fs.lookup(parent_inode, name.to_str().unwrap()) {
            return reply.error(ErrCode::EEXIST as i32);
        }
        match self.fs.mkdir(
            parent_inode,
            name.to_str().unwrap(),
            InodeMode::from_bits_truncate(mode as u16),
        ) {
            Ok(ino) => {
                self.retain_lookup(ino);
                reply.entry(&get_ttl(), &self.get_attr(ino).unwrap(), 0)
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn opendir(&mut self, _req: &Request<'_>, ino: u64, _flags: i32, reply: ReplyOpen) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        match self.get_attr(inode) {
            Ok(attr) => {
                if attr.kind != FileType::Directory {
                    return reply.error(ErrCode::ENOTDIR as i32);
                }
                let fh = fuse_result!(reply, self.allocate_handle(true));
                self.open_dir_handles.insert(fh, self.handle(ino));
                reply.opened(fh, 0);
            }
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn readdir(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        let inode = fuse_result!(reply, self.check_handle(&self.open_dir_handles, fh, ino));
        let entries = self.fs.listdir(inode);
        match entries {
            Ok(entries) => {
                let mut i = offset as usize;
                while i < entries.len() {
                    let entry = &entries[i];
                    let attr = fuse_result!(
                        reply,
                        self.fs
                            .getattr(entry.inode())
                            .map_err(|error| error.code() as i32)
                    );
                    if reply.add(
                        self.nodes.encode(entry.inode()),
                        i as i64 + 1,
                        translate_ftype(attr.ftype),
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
        ino: u64,
        fh: u64,
        _flags: i32,
        reply: ReplyEmpty,
    ) {
        match self.release_handle(fh, ino, true) {
            Ok(()) => reply.ok(),
            Err(error) => reply.error(error),
        }
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let parent_inode = fuse_result!(reply, self.nodes.decode(parent));
        match self.fs.rmdir(parent_inode, name.to_str().unwrap()) {
            Ok(Some(handle)) => match self.queue_reclaim(handle) {
                Ok(()) => reply.ok(),
                Err(e) => reply.error(e.code() as i32),
            },
            Ok(None) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn access(&mut self, req: &Request<'_>, ino: u64, mask: i32, reply: ReplyEmpty) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let attr = fuse_result!(reply, self.fs.getattr(inode).map_err(|e| e.code() as i32));
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
        ino: u64,
        fh: u64,
        _flags: u32,
        cmd: u32,
        in_data: &[u8],
        _out_size: u32,
        reply: fuser::ReplyIoctl,
    ) {
        // Root directory handles are stable control handles across restores.
        // Other nodes must belong to the current timeline.
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let handles = if _flags & fuser::consts::FUSE_IOCTL_DIR != 0 {
            &self.open_dir_handles
        } else {
            &self.open_handles
        };
        if inode == 1 {
            if !handles.get(&fh).is_some_and(|handle| handle.node == ino) {
                return reply.error(libc::ESTALE);
            }
        } else {
            fuse_result!(reply, self.check_handle(handles, fh, ino));
        }
        let result = match Self::snapshot_key(cmd, in_data) {
            Ok((restore, key)) => {
                if restore {
                    self.restore(key)
                } else {
                    self.checkpoint(key)
                }
            }
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => reply.ioctl(0, &[]),
            Err(error) => reply.error(error),
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
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(size as usize).is_err() {
            return reply.error(ErrCode::ENOMEM as i32);
        }
        buffer.resize(size as usize, 0);
        match self.fs.getxattr(inode, name.as_bytes(), &mut buffer) {
            Ok(count) if size == 0 => reply.size(count as u32),
            Ok(count) => reply.data(&buffer[..count]),
            Err(error) => reply.error(error.code() as i32),
        }
    }

    fn setxattr(
        &mut self,
        _req: &Request<'_>,
        ino: u64,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        _position: u32,
        reply: ReplyEmpty,
    ) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let name = name.as_bytes();
        const XATTR_CREATE: i32 = 1;
        const XATTR_REPLACE: i32 = 2;
        if flags & !(XATTR_CREATE | XATTR_REPLACE) != 0 {
            return reply.error(ErrCode::EINVAL as i32);
        }
        match self.fs.setxattr_with_flags(
            inode,
            name,
            value,
            flags & XATTR_CREATE != 0,
            flags & XATTR_REPLACE != 0,
        ) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn removexattr(&mut self, _req: &Request<'_>, ino: u64, name: &OsStr, reply: ReplyEmpty) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        match self.fs.removexattr(inode, name.as_bytes()) {
            Ok(_) => reply.ok(),
            Err(e) => reply.error(e.code() as i32),
        }
    }

    fn listxattr(&mut self, _req: &Request<'_>, ino: u64, size: u32, reply: fuser::ReplyXattr) {
        let inode = fuse_result!(reply, self.nodes.decode(ino));
        let mut buffer = Vec::new();
        if buffer.try_reserve_exact(size as usize).is_err() {
            return reply.error(ErrCode::ENOMEM as i32);
        }
        buffer.resize(size as usize, 0);
        match self.fs.listxattr(inode, &mut buffer) {
            Ok(count) if size == 0 => reply.size(count as u32),
            Ok(count) => reply.data(&buffer[..count]),
            Err(error) => reply.error(error.code() as i32),
        }
    }
}

fn get_ttl() -> Duration {
    Duration::ZERO
}

#[cfg(test)]
mod snapshot_tests;
