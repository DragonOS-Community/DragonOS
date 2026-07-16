//! FUSE protocol structures (Linux 6.6 uapi compatible subset).
//!
//! Reference: linux-6.6.21 `include/uapi/linux/fuse.h`.

use core::mem::size_of;

use system_error::SystemError;

pub const FUSE_KERNEL_VERSION: u32 = 7;
pub const FUSE_KERNEL_MINOR_VERSION: u32 = 39;

/// The read buffer is required to be at least 8k on Linux.
pub const FUSE_MIN_READ_BUFFER: usize = 8192;

pub const FUSE_ROOT_ID: u64 = 1;

// Opcodes (subset)
pub const FUSE_LOOKUP: u32 = 1;
pub const FUSE_FORGET: u32 = 2; // no reply
pub const FUSE_GETATTR: u32 = 3;
pub const FUSE_SETATTR: u32 = 4;
pub const FUSE_READLINK: u32 = 5;
pub const FUSE_SYMLINK: u32 = 6;
pub const FUSE_MKNOD: u32 = 8;
pub const FUSE_MKDIR: u32 = 9;
pub const FUSE_UNLINK: u32 = 10;
pub const FUSE_RMDIR: u32 = 11;
pub const FUSE_RENAME: u32 = 12;
pub const FUSE_LINK: u32 = 13;
pub const FUSE_OPEN: u32 = 14;
pub const FUSE_READ: u32 = 15;
pub const FUSE_WRITE: u32 = 16;
pub const FUSE_STATFS: u32 = 17;
pub const FUSE_RELEASE: u32 = 18;
pub const FUSE_FSYNC: u32 = 20;
pub const FUSE_SETXATTR: u32 = 21;
pub const FUSE_GETXATTR: u32 = 22;
pub const FUSE_LISTXATTR: u32 = 23;
pub const FUSE_REMOVEXATTR: u32 = 24;
pub const FUSE_FLUSH: u32 = 25;
pub const FUSE_INIT: u32 = 26;
pub const FUSE_OPENDIR: u32 = 27;
pub const FUSE_READDIR: u32 = 28;
pub const FUSE_RELEASEDIR: u32 = 29;
pub const FUSE_FSYNCDIR: u32 = 30;
pub const FUSE_ACCESS: u32 = 34;
pub const FUSE_CREATE: u32 = 35;
pub const FUSE_INTERRUPT: u32 = 36;
pub const FUSE_DESTROY: u32 = 38; // no reply
pub const FUSE_FALLOCATE: u32 = 43;
pub const FUSE_READDIRPLUS: u32 = 44;
pub const FUSE_RENAME2: u32 = 45;
pub const FUSE_SETUPMAPPING: u32 = 48;
pub const FUSE_REMOVEMAPPING: u32 = 49;

// INIT flags (subset)
pub const FUSE_ASYNC_READ: u64 = 1 << 0;
pub const FUSE_POSIX_LOCKS: u64 = 1 << 1;
pub const FUSE_ATOMIC_O_TRUNC: u64 = 1 << 3;
pub const FUSE_EXPORT_SUPPORT: u64 = 1 << 4;
pub const FUSE_BIG_WRITES: u64 = 1 << 5;
pub const FUSE_DONT_MASK: u64 = 1 << 6;
pub const FUSE_AUTO_INVAL_DATA: u64 = 1 << 12;
pub const FUSE_DO_READDIRPLUS: u64 = 1 << 13;
pub const FUSE_READDIRPLUS_AUTO: u64 = 1 << 14;
pub const FUSE_ASYNC_DIO: u64 = 1 << 15;
#[allow(dead_code)]
pub const FUSE_WRITEBACK_CACHE: u64 = 1 << 16;
pub const FUSE_NO_OPEN_SUPPORT: u64 = 1 << 17;
pub const FUSE_PARALLEL_DIROPS: u64 = 1 << 18;
pub const FUSE_HANDLE_KILLPRIV: u64 = 1 << 19;
pub const FUSE_POSIX_ACL: u64 = 1 << 20;
pub const FUSE_ABORT_ERROR: u64 = 1 << 21;
pub const FUSE_MAX_PAGES: u64 = 1 << 22;
pub const FUSE_NO_OPENDIR_SUPPORT: u64 = 1 << 24;
pub const FUSE_EXPLICIT_INVAL_DATA: u64 = 1 << 25;
pub const FUSE_MAP_ALIGNMENT: u64 = 1 << 26;
/// Guest auto-mounts directories marked FUSE_ATTR_SUBMOUNT (Linux fuse.h: init->flags bit 27).
pub const FUSE_SUBMOUNTS: u64 = 1 << 27;
pub const FUSE_INIT_EXT: u64 = 1 << 30;
pub const FUSE_HAS_INODE_DAX: u64 = 1 << 33;
/// Kernel supports expiry-only entry invalidation (Linux 6.6 fuse.h bit 35).
pub const FUSE_HAS_EXPIRE_ONLY: u64 = 1 << 35;
/// Allow shared mmap for FOPEN_DIRECT_IO files (Linux 6.6 fuse.h bit 36).
#[allow(dead_code)]
pub const FUSE_DIRECT_IO_ALLOW_MMAP: u64 = 1 << 36;

/// fuse_attr.flags (Linux 6.6): directory is a submount root announced by virtiofsd.
pub const FUSE_ATTR_SUBMOUNT: u32 = 1 << 0;
/// fuse_attr.flags (Linux 6.6): enable DAX for this regular file in inode mode.
pub const FUSE_ATTR_DAX: u32 = 1 << 1;

// fuse_getattr_in.getattr_flags (Linux 6.6 uapi subset)
pub const FUSE_GETATTR_FH: u32 = 1 << 0;

// fuse_open_out.open_flags (Linux 6.6 uapi subset)
pub const FOPEN_DIRECT_IO: u32 = 1 << 0;
pub const FOPEN_KEEP_CACHE: u32 = 1 << 1;
#[allow(dead_code)]
pub const FOPEN_NONSEEKABLE: u32 = 1 << 2;
#[allow(dead_code)]
pub const FOPEN_CACHE_DIR: u32 = 1 << 3;
#[allow(dead_code)]
pub const FOPEN_STREAM: u32 = 1 << 4;
#[allow(dead_code)]
pub const FOPEN_NOFLUSH: u32 = 1 << 5;
#[allow(dead_code)]
pub const FOPEN_PARALLEL_DIRECT_WRITES: u32 = 1 << 6;

// fuse_write_in.write_flags (Linux 6.6 uapi subset)
pub const FUSE_WRITE_CACHE: u32 = 1 << 0;
pub const FUSE_WRITE_LOCKOWNER: u32 = 1 << 1;

// fuse_read_in.read_flags (Linux 6.6 uapi subset)
pub const FUSE_READ_LOCKOWNER: u32 = 1 << 1;

// getattr/setattr valid bits (subset)
pub const FATTR_MODE: u32 = 1 << 0;
pub const FATTR_UID: u32 = 1 << 1;
pub const FATTR_GID: u32 = 1 << 2;
pub const FATTR_SIZE: u32 = 1 << 3;
pub const FATTR_ATIME: u32 = 1 << 4;
pub const FATTR_MTIME: u32 = 1 << 5;
#[allow(dead_code)]
pub const FATTR_FH: u32 = 1 << 6;
#[allow(dead_code)]
pub const FATTR_ATIME_NOW: u32 = 1 << 7;
#[allow(dead_code)]
pub const FATTR_MTIME_NOW: u32 = 1 << 8;
pub const FATTR_LOCKOWNER: u32 = 1 << 9;
pub const FATTR_CTIME: u32 = 1 << 10;

pub const FUSE_FSYNC_FDATASYNC: u32 = 1 << 0;

pub const FUSE_SETUPMAPPING_FLAG_WRITE: u64 = 1 << 0;
pub const FUSE_SETUPMAPPING_FLAG_READ: u64 = 1 << 1;
pub const FUSE_NOTIFY_POLL: i32 = 1;
pub const FUSE_NOTIFY_INVAL_INODE: i32 = 2;
pub const FUSE_NOTIFY_INVAL_ENTRY: i32 = 3;
pub const FUSE_NOTIFY_STORE: i32 = 4;
pub const FUSE_NOTIFY_RETRIEVE: i32 = 5;
pub const FUSE_NOTIFY_DELETE: i32 = 6;
pub const FUSE_EXPIRE_ONLY: u32 = 1 << 0;

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseNotifyInvalInodeOut {
    pub ino: u64,
    pub off: i64,
    pub len: i64,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseNotifyInvalEntryOut {
    pub parent: u64,
    pub namelen: u32,
    pub flags: u32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseNotifyDeleteOut {
    pub parent: u64,
    pub child: u64,
    pub namelen: u32,
    pub padding: u32,
}
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInHeader {
    pub len: u32,
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
    pub uid: u32,
    pub gid: u32,
    pub pid: u32,
    pub total_extlen: u16,
    pub padding: u16,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOutHeader {
    pub len: u32,
    pub error: i32,
    pub unique: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitIn {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub flags2: u32,
    pub unused: [u32; 11],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInitOut {
    pub major: u32,
    pub minor: u32,
    pub max_readahead: u32,
    pub flags: u32,
    pub max_background: u16,
    pub congestion_threshold: u16,
    pub max_write: u32,
    pub time_gran: u32,
    pub max_pages: u16,
    pub map_alignment: u16,
    pub flags2: u32,
    pub unused: [u32; 7],
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseSetupMappingIn {
    pub fh: u64,
    pub foffset: u64,
    pub len: u64,
    pub flags: u64,
    pub moffset: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseRemoveMappingIn {
    pub count: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FuseRemoveMappingOne {
    pub moffset: u64,
    pub len: u64,
}

const _: [(); 40] = [(); size_of::<FuseSetupMappingIn>()];
const _: [(); 4] = [(); size_of::<FuseRemoveMappingIn>()];
const _: [(); 16] = [(); size_of::<FuseRemoveMappingOne>()];

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAttr {
    pub ino: u64,
    pub size: u64,
    pub blocks: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub nlink: u32,
    pub uid: u32,
    pub gid: u32,
    pub rdev: u32,
    pub blksize: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseEntryOut {
    pub nodeid: u64,
    pub generation: u64,
    pub entry_valid: u64,
    pub attr_valid: u64,
    pub entry_valid_nsec: u32,
    pub attr_valid_nsec: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseForgetIn {
    pub nlookup: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseInterruptIn {
    pub unique: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseGetattrIn {
    pub getattr_flags: u32,
    pub dummy: u32,
    pub fh: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAttrOut {
    pub attr_valid: u64,
    pub attr_valid_nsec: u32,
    pub dummy: u32,
    pub attr: FuseAttr,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOpenIn {
    pub flags: u32,
    pub open_flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseOpenOut {
    pub fh: u64,
    pub open_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub read_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseWriteIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
    pub write_flags: u32,
    pub lock_owner: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseWriteOut {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseFallocateIn {
    pub fh: u64,
    pub offset: u64,
    pub length: u64,
    pub mode: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseKstatfs {
    pub blocks: u64,
    pub bfree: u64,
    pub bavail: u64,
    pub files: u64,
    pub ffree: u64,
    pub bsize: u32,
    pub namelen: u32,
    pub frsize: u32,
    pub padding: u32,
    pub spare: [u32; 6],
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseStatfsOut {
    pub st: FuseKstatfs,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseReleaseIn {
    pub fh: u64,
    pub flags: u32,
    pub release_flags: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseMknodIn {
    pub mode: u32,
    pub rdev: u32,
    pub umask: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseMkdirIn {
    pub mode: u32,
    pub umask: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseRenameIn {
    pub newdir: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseRename2In {
    pub newdir: u64,
    pub flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseLinkIn {
    pub oldnodeid: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseSetattrIn {
    pub valid: u32,
    pub padding: u32,
    pub fh: u64,
    pub size: u64,
    pub lock_owner: u64,
    pub atime: u64,
    pub mtime: u64,
    pub ctime: u64,
    pub atimensec: u32,
    pub mtimensec: u32,
    pub ctimensec: u32,
    pub mode: u32,
    pub unused4: u32,
    pub uid: u32,
    pub gid: u32,
    pub unused5: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseDirent {
    pub ino: u64,
    pub off: u64,
    pub namelen: u32,
    pub typ: u32,
    // name follows
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseDirentPlus {
    pub entry_out: FuseEntryOut,
    pub dirent: FuseDirent,
    // name follows
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseCreateIn {
    pub flags: u32,
    pub mode: u32,
    pub umask: u32,
    pub open_flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseFlushIn {
    pub fh: u64,
    pub unused: u32,
    pub padding: u32,
    pub lock_owner: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseFsyncIn {
    pub fh: u64,
    pub fsync_flags: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseSetxattrInCompat {
    pub size: u32,
    pub flags: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseGetxattrIn {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseGetxattrOut {
    pub size: u32,
    pub padding: u32,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct FuseAccessIn {
    pub mask: u32,
    pub padding: u32,
}

pub fn fuse_pack_struct<T: Copy>(v: &T) -> &[u8] {
    unsafe { core::slice::from_raw_parts((v as *const T).cast::<u8>(), size_of::<T>()) }
}

pub fn fuse_read_struct<T: Copy>(buf: &[u8]) -> Result<T, SystemError> {
    if buf.len() < size_of::<T>() {
        return Err(SystemError::EINVAL);
    }
    // FUSE messages are packed and 64-bit aligned, but userspace may still
    // pass unaligned buffers; use unaligned reads for robustness.
    Ok(unsafe { core::ptr::read_unaligned(buf.as_ptr().cast::<T>()) })
}
