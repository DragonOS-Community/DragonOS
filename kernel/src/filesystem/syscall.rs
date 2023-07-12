

use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::FileType,
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

pub type DevType = u32;
pub type LoffType = i64;
bitflags! {
    /// 文件类型和权限
    pub struct ModeType: u32 {
        /// 掩码
        const S_IFMT = 0x0170000;

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

        const S_IRWXU = 0o0700;
        const S_IRUSR = 0o0400;
        const S_IWUSER = 0o0100;

        const S_IRWXG = 0o0070;
        const S_IRGRP = 0o0040;
        const S_IWGRP = 0o0020;
        const S_IXGRP = 0o0010;

        const S_IRWXO = 0o0007;
        const S_IROTH = 0o0004;
        const S_IWOTH = 0o0002;
        const S_IXOTH = 0o0001;
    }
}
pub struct Kstat {
    inode: u64,
    dev_id: DevType,
    mode: ModeType,
    nlink: u32,
    uid: u32,
    gid: u32,
    rdev: DevType,
    size: LoffType,
    atime: TimeSpec,
    mtime: TimeSpec,
    ctime: TimeSpec,
    blcok_size: u64,
    blocks: u64,
}
impl Default for Kstat {
    fn default() -> Self {
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
        }
    }
}
impl Syscall {
    pub fn vfs_fstat(fd: i32) {}
    pub fn do_fstat(fd: i32) -> Result<Kstat, SystemError> {
        let cur = current_pcb();
        match cur.get_file_ref_by_fd(fd) {
            Some(file) => {
                let mut kstat = Kstat::default();

                match file.metadata() {
                    Ok(matedata) => {
                        kstat.size = matedata.size;
                        kstat.dev_id = matedata.dev_id as u32;
                        kstat.inode = matedata.inode_id as u64;
                        kstat.blcok_size = matedata.blk_size as u64;
                        kstat.blocks = matedata.blocks as u64;

                        kstat.atime.tv_sec = matedata.atime.tv_sec;
                        kstat.atime.tv_nsec = matedata.atime.tv_nsec;
                        kstat.mtime.tv_sec = matedata.mtime.tv_sec;
                        kstat.mtime.tv_nsec = matedata.mtime.tv_nsec;
                        kstat.ctime.tv_sec = matedata.ctime.tv_sec;
                        kstat.ctime.tv_nsec = matedata.ctime.tv_nsec;

                        kstat.nlink = matedata.nlinks as u32;
                        kstat.uid = matedata.uid as u32;
                        kstat.gid = matedata.gid as u32;
                        kstat.rdev = matedata.raw_dev as u32;
                        // TODO 给mode赋值
                        kstat.mode.bits = matedata.mode;
                        match file.file_type() {
                            FileType::File => kstat.mode.insert(ModeType::S_IFMT),
                            FileType::Dir => kstat.mode.insert(ModeType::S_IFDIR),
                            FileType::BlockDevice => kstat.mode.insert(ModeType::S_IFBLK),
                            FileType::CharDevice => kstat.mode.insert(ModeType::S_IFCHR),
                            FileType::SymLink => kstat.mode.insert(ModeType::S_IFLNK),
                            FileType::Socket => kstat.mode.insert(ModeType::S_IFSOCK),
                            FileType::Pipe => kstat.mode.insert(ModeType::S_IFIFO),
                        }
                    }
                    Err(e) => return Err(e),
                }

                return Ok(kstat);
            }
            None => {
                return Err(SystemError::EINVAL);
            }
        }
    }
    pub fn fstat(mut usr_kstat: *mut Kstat, fd: i32) -> Result<i32, SystemError> {
        match Self::do_fstat(fd) {
            Ok(kstat) => {
                // TODO 给传出指针赋值
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
    }
}
