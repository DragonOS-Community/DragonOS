use crate::{
    filesystem::vfs::FileType,
    kdebug,
    libs::rwlock::RwLock,
    process::ProcessManager,
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

use super::vfs::file::FileDescriptorVec;

bitflags! {
    /// 文件类型和权限
    pub struct ModeType: u32 {
        /// 掩码
        const S_IFMT = 0o0_170_000;
        /// 文件类型
        const S_IFSOCK = 0o140000;
        const S_IFLNK = 0o120000;
        const S_IFREG = 0o100000;
        const S_IFBLK = 0o060000;
        const S_IFDIR = 0o040000;
        const S_IFCHR = 0o020000;
        const S_IFIFO = 0o010000;

        const S_ISUID = 0o004000;
        const S_ISGID = 0o002000;
        const S_ISVTX = 0o001000;
        /// 文件用户权限
        const S_IRWXU = 0o0700;
        const S_IRUSR = 0o0400;
        const S_IWUSR = 0o0200;
        const S_IXUSR = 0o0100;
        /// 文件组权限
        const S_IRWXG = 0o0070;
        const S_IRGRP = 0o0040;
        const S_IWGRP = 0o0020;
        const S_IXGRP = 0o0010;
        /// 文件其他用户权限
        const S_IRWXO = 0o0007;
        const S_IROTH = 0o0004;
        const S_IWOTH = 0o0002;
        const S_IXOTH = 0o0001;
    }
}

#[repr(C)]
/// # 文件信息结构体
pub struct PosixKstat {
    /// 硬件设备ID
    dev_id: u64,
    /// inode号
    inode: u64,
    /// 硬链接数
    nlink: u64,
    /// 文件权限
    mode: ModeType,
    /// 所有者用户ID
    uid: i32,
    /// 所有者组ID
    gid: i32,
    /// 设备ID
    rdev: i64,
    /// 文件大小
    size: i64,
    /// 文件系统块大小
    blcok_size: i64,
    /// 分配的512B块数
    blocks: u64,
    /// 最后访问时间
    atime: TimeSpec,
    /// 最后修改时间
    mtime: TimeSpec,
    /// 最后状态变化时间
    ctime: TimeSpec,
    /// 用于填充结构体大小的空白数据
    pub _pad: [i8; 24],
}
impl PosixKstat {
    fn new() -> Self {
        Self {
            inode: 0,
            dev_id: 0,
            mode: ModeType { bits: 0 },
            nlink: 0,
            uid: 0,
            gid: 0,
            rdev: 0,
            size: 0,
            atime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            mtime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            ctime: TimeSpec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            blcok_size: 0,
            blocks: 0,
            _pad: Default::default(),
        }
    }
}
impl Syscall {
    fn do_fstat(fd: i32) -> Result<PosixKstat, SystemError> {
        let binding = ProcessManager::current_pcb();
        let fd_table: alloc::sync::Arc<RwLock<FileDescriptorVec>> = binding
            .basic()
            .fd_table()
            .unwrap_or_else(|| panic!("pid: {:?} fd_table is none", binding.basic().pid()));
        let cur = fd_table.read();
        match cur.get_file_ref_by_fd(fd) {
            Some(file) => {
                let mut kstat = PosixKstat::new();
                // 获取文件信息
                let metadata = file.metadata()?;
                kstat.size = metadata.size as i64;
                kstat.dev_id = metadata.dev_id as u64;
                kstat.inode = metadata.inode_id as u64;
                kstat.blcok_size = metadata.blk_size as i64;
                kstat.blocks = metadata.blocks as u64;

                kstat.atime.tv_sec = metadata.atime.tv_sec;
                kstat.atime.tv_nsec = metadata.atime.tv_nsec;
                kstat.mtime.tv_sec = metadata.mtime.tv_sec;
                kstat.mtime.tv_nsec = metadata.mtime.tv_nsec;
                kstat.ctime.tv_sec = metadata.ctime.tv_sec;
                kstat.ctime.tv_nsec = metadata.ctime.tv_nsec;

                kstat.nlink = metadata.nlinks as u64;
                kstat.uid = metadata.uid as i32;
                kstat.gid = metadata.gid as i32;
                kstat.rdev = metadata.raw_dev as i64;
                kstat.mode.bits = metadata.mode;
                match file.file_type() {
                    FileType::File => kstat.mode.insert(ModeType::S_IFMT),
                    FileType::Dir => kstat.mode.insert(ModeType::S_IFDIR),
                    FileType::BlockDevice => kstat.mode.insert(ModeType::S_IFBLK),
                    FileType::CharDevice => kstat.mode.insert(ModeType::S_IFCHR),
                    FileType::SymLink => kstat.mode.insert(ModeType::S_IFLNK),
                    FileType::Socket => kstat.mode.insert(ModeType::S_IFSOCK),
                    FileType::Pipe => kstat.mode.insert(ModeType::S_IFIFO),
                }

                return Ok(kstat);
            }
            None => {
                kdebug!("file not be opened");
                return Err(SystemError::EINVAL);
            }
        }
    }
    pub fn fstat(fd: i32, usr_kstat: *mut PosixKstat) -> Result<usize, SystemError> {
        match Self::do_fstat(fd) {
            Ok(kstat) => {
                if usr_kstat.is_null() {
                    return Err(SystemError::EFAULT);
                }
                unsafe {
                    *usr_kstat = kstat;
                }
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
    }
}
