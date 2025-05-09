use system_error::SystemError;

use crate::filesystem::vfs::stat::KStat;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct PosixStat {
    pub st_dev: usize,
    pub st_ino: usize,
    pub st_nlink: usize,
    pub st_mode: u32,
    pub st_uid: u32,
    pub st_gid: u32,
    pub __pad0: u32,
    pub st_rdev: usize,
    pub st_size: isize,
    pub st_blksize: isize,
    /// number of 512B blocks allocated
    pub st_blocks: isize,
    pub st_atime: usize,
    pub st_atime_nsec: usize,
    pub st_mtime: usize,
    pub st_mtime_nsec: usize,
    pub st_ctime: usize,
    pub st_ctime_nsec: usize,
    pub __unused: [isize; 3],
}

/// 转换的代码参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#393
impl TryFrom<KStat> for PosixStat {
    type Error = SystemError;

    fn try_from(kstat: KStat) -> Result<Self, Self::Error> {
        let mut tmp = PosixStat::default();
        if core::mem::size_of_val(&tmp.st_dev) < 4 && !kstat.dev.old_valid_dev() {
            return Err(SystemError::EOVERFLOW);
        }
        if core::mem::size_of_val(&tmp.st_rdev) < 4 && !kstat.rdev.old_valid_dev() {
            return Err(SystemError::EOVERFLOW);
        }

        tmp.st_dev = kstat.dev.new_encode_dev() as usize;
        tmp.st_ino = kstat.ino as usize;

        if core::mem::size_of_val(&tmp.st_ino) < core::mem::size_of_val(&kstat.ino)
            && tmp.st_ino != kstat.ino as usize
        {
            return Err(SystemError::EOVERFLOW);
        }

        tmp.st_mode = kstat.mode.bits();
        tmp.st_nlink = kstat.nlink.try_into().map_err(|_| SystemError::EOVERFLOW)?;

        // todo: 处理user namespace (https://code.dragonos.org.cn/xref/linux-6.6.21/fs/stat.c#415)
        tmp.st_uid = kstat.uid;
        tmp.st_gid = kstat.gid;

        tmp.st_rdev = kstat.rdev.data() as usize;
        tmp.st_size = kstat.size as isize;

        tmp.st_atime = kstat.atime.tv_sec as usize;
        tmp.st_mtime = kstat.mtime.tv_sec as usize;
        tmp.st_ctime = kstat.ctime.tv_sec as usize;
        tmp.st_atime_nsec = kstat.atime.tv_nsec as usize;
        tmp.st_mtime_nsec = kstat.mtime.tv_nsec as usize;
        tmp.st_ctime_nsec = kstat.ctime.tv_nsec as usize;
        tmp.st_blocks = kstat.blocks as isize;
        tmp.st_blksize = kstat.blksize as isize;

        Ok(tmp)
    }
}
