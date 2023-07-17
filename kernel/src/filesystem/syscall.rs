use crate::{
    arch::asm::current::current_pcb,
    filesystem::vfs::FileType,
    kdebug,
    syscall::{Syscall, SystemError},
    time::TimeSpec,
};

bitflags! {
    /// 文件类型和权限
    pub struct ModeType: u32 {
        /// 掩码
        const S_IFMT = 0o0_170_000;

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

#[repr(C)]
pub struct PosixKstat {
    dev_id: u64,
    inode: u64,
    nlink: u64,
    mode: ModeType,
    uid: i32,
    gid: i32,
    rdev: i64,
    size: i64,
    blcok_size: i64,
    blocks: u64,

    atime: TimeSpec,
    mtime: TimeSpec,
    ctime: TimeSpec,
    pub _pad: [i8; 24],
}
impl Default for PosixKstat {
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
            _pad: Default::default(),
        }
    }
}
impl Syscall {
    pub fn do_fstat(fd: i32) -> Result<PosixKstat, SystemError> {
        let cur = current_pcb();
        kdebug!("kfd = {:?}", fd);
        match cur.get_file_ref_by_fd(fd) {
            Some(file) => {
                kdebug!("file is gotten");
                let mut kstat = PosixKstat::default();

                match file.metadata() {
                    Ok(matedata) => {
                        kstat.size = matedata.size as i64;
                        kstat.dev_id = matedata.dev_id as u64;
                        kstat.inode = matedata.inode_id as u64;
                        kstat.blcok_size = matedata.blk_size as i64;
                        kstat.blocks = matedata.blocks as u64;

                        kstat.atime.tv_sec = matedata.atime.tv_sec;
                        kstat.atime.tv_nsec = matedata.atime.tv_nsec;
                        kstat.mtime.tv_sec = matedata.mtime.tv_sec;
                        kstat.mtime.tv_nsec = matedata.mtime.tv_nsec;
                        kstat.ctime.tv_sec = matedata.ctime.tv_sec;
                        kstat.ctime.tv_nsec = matedata.ctime.tv_nsec;

                        kstat.nlink = matedata.nlinks as u64;
                        kstat.uid = matedata.uid as i32;
                        kstat.gid = matedata.gid as i32;
                        kstat.rdev = matedata.raw_dev as i64;
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
                        kdebug!(
                            "kstat\ndev_id = {:?}\ninode = {:?}\nnlink = {:?}\n
                            mode = {:?}\nuid = {:?}\ngid = {:?}\nrdev = {:?}\n
                            size = {:?}\nblcok_size = {:?}\nblocks = {:?}\n
                            atime.sec = {:?} nsec = {:?}\n
                            mtime.sec = {:?} nsec = {:?}\n
                            ctime.sec = {:?} nsec = {:?}\n",
                            kstat.dev_id,
                            kstat.inode,
                            kstat.nlink,
                            kstat.mode,
                            kstat.uid,
                            kstat.gid,
                            kstat.rdev,
                            kstat.size,
                            kstat.blcok_size,
                            kstat.blocks,
                            kstat.atime.tv_sec,
                            kstat.atime.tv_nsec,
                            kstat.mtime.tv_sec,
                            kstat.mtime.tv_nsec,
                            kstat.ctime.tv_sec,
                            kstat.ctime.tv_nsec,
                        );
                    }
                    Err(e) => return Err(e),
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
                    kdebug!("usr_kstat = {:p}", usr_kstat);
                }
                // unsafe {
                //     kdebug!(
                //         "(*usr_kstat)\ndev_id = {:?}\ninode = {:?}\nnlink = {:?}\n
                //             mode = {:?}\nuid = {:?}\ngid = {:?}\nrdev = {:?}\n
                //             size = {:?}\nblcok_size = {:?}\nblocks = {:?}\n
                //             atime.sec = {:?} nsec = {:?}\n
                //             mtime.sec = {:?} nsec = {:?}\n
                //             ctime.sec = {:?} nsec = {:?}\n",
                //         (*usr_kstat).dev_id,
                //         (*usr_kstat).inode,
                //         (*usr_kstat).nlink,
                //         (*usr_kstat).mode,
                //         (*usr_kstat).uid,
                //         (*usr_kstat).gid,
                //         (*usr_kstat).rdev,
                //         (*usr_kstat).size,
                //         (*usr_kstat).blcok_size,
                //         (*usr_kstat).blocks,
                //         (*usr_kstat).atime.tv_sec,
                //         (*usr_kstat).atime.tv_nsec,
                //         (*usr_kstat).mtime.tv_sec,
                //         (*usr_kstat).mtime.tv_nsec,
                //         (*usr_kstat).ctime.tv_sec,
                //         (*usr_kstat).ctime.tv_nsec,
                //     );
                // }
                return Ok(0);
            }
            Err(e) => return Err(e),
        }
    }
}
