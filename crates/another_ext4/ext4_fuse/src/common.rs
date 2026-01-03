use another_ext4::{FileAttr as Ext4FileAttr, FileType as Ext4FileType, INODE_BLOCK_SIZE};
use fuser::{FileAttr, FileType, TimeOrNow};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub fn translate_ftype(file_type: Ext4FileType) -> FileType {
    match file_type {
        Ext4FileType::RegularFile => FileType::RegularFile,
        Ext4FileType::Directory => FileType::Directory,
        Ext4FileType::CharacterDev => FileType::CharDevice,
        Ext4FileType::BlockDev => FileType::BlockDevice,
        Ext4FileType::Fifo => FileType::NamedPipe,
        Ext4FileType::Socket => FileType::Socket,
        Ext4FileType::SymLink => FileType::Symlink,
        Ext4FileType::Unknown => FileType::RegularFile,
    }
}

pub fn translate_attr(attr: Ext4FileAttr) -> FileAttr {
    FileAttr {
        ino: attr.ino as u64,
        size: attr.size,
        blocks: attr.blocks,
        atime: second2sys_time(attr.atime),
        mtime: second2sys_time(attr.mtime),
        ctime: second2sys_time(attr.ctime),
        crtime: second2sys_time(attr.crtime),
        kind: translate_ftype(attr.ftype),
        perm: attr.perm.bits(),
        nlink: attr.links as u32,
        uid: attr.uid,
        gid: attr.gid,
        rdev: 0,
        blksize: INODE_BLOCK_SIZE as u32,
        flags: 0,
    }
}

pub fn sys_time2second(time: SystemTime) -> u32 {
    time.duration_since(UNIX_EPOCH).unwrap().as_secs() as u32
}

pub fn second2sys_time(time: u32) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(time as u64)
}

pub fn time_or_now2second(time_or_now: TimeOrNow) -> u32 {
    match time_or_now {
        fuser::TimeOrNow::Now => sys_time2second(SystemTime::now()),
        fuser::TimeOrNow::SpecificTime(time) => sys_time2second(time),
    }
}
