use core::any::Any;

use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::filesystem::{
    epoll::{EPollEventType, EPollItem},
    vfs::{
        file::{File, FileFlags, FilePrivateData, ReservedFd},
        vcore::generate_inode_id,
        FileSystem, FileType, FsInfo, IndexNode, InodeFlags, InodeMode, Magic, Metadata,
        PollableInode, SuperBlock,
    },
};
use crate::libs::mutex::MutexGuard;
use crate::mm::MemoryManagementArch;

use super::{
    pid::{Pid, PidPrivateData, PidType},
    ProcessControlBlock,
};

lazy_static::lazy_static! {
    static ref PIDFD_FS: Arc<PidFdFs> = Arc::new(PidFdFs);
}

#[derive(Debug)]
pub struct PidFdFs;

impl PidFdFs {
    fn instance() -> Arc<Self> {
        PIDFD_FS.clone()
    }
}

impl FileSystem for PidFdFs {
    fn root_inode(&self) -> Arc<dyn IndexNode> {
        Arc::new(PidFdInode::new())
    }

    fn info(&self) -> FsInfo {
        FsInfo {
            blk_dev_id: 0,
            max_name_len: 255,
        }
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn name(&self) -> &str {
        "pidfd"
    }

    fn super_block(&self) -> SuperBlock {
        SuperBlock::new(
            Magic::PIDFD_MAGIC,
            <crate::arch::MMArch as MemoryManagementArch>::PAGE_SIZE as u64,
            255,
        )
    }
}

#[derive(Debug)]
pub struct PidFdInode {
    metadata: Metadata,
}

impl PidFdInode {
    fn new() -> Self {
        Self {
            metadata: Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: crate::time::PosixTimeSpec::default(),
                mtime: crate::time::PosixTimeSpec::default(),
                ctime: crate::time::PosixTimeSpec::default(),
                btime: crate::time::PosixTimeSpec::default(),
                file_type: FileType::File,
                mode: InodeMode::from_bits_truncate(0o600),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: Default::default(),
                flags: InodeFlags::empty(),
            },
        }
    }
}

impl IndexNode for PidFdInode {
    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        Ok(())
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        Ok(())
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        PidFdFs::instance()
    }

    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::EINVAL)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        Ok(self.metadata.clone())
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(String::from("anon_inode:[pidfd]"))
    }

    fn as_pollable_inode(&self) -> Result<&dyn PollableInode, SystemError> {
        Ok(self)
    }
}

impl PollableInode for PidFdInode {
    fn poll(&self, private_data: &FilePrivateData) -> Result<usize, SystemError> {
        let FilePrivateData::Pid(pid_data) = private_data else {
            return Err(SystemError::EBADF);
        };
        let exited = pid_data.pid().thread_group_exited_for_pidfd();
        if exited {
            Ok((EPollEventType::EPOLLIN | EPollEventType::EPOLLRDNORM).bits() as usize)
        } else {
            Ok(0)
        }
    }

    fn add_epitem(
        &self,
        epitem: Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::Pid(pid_data) = private_data else {
            return Err(SystemError::EBADF);
        };
        pid_data.pid().add_pidfd_epitem(epitem);
        Ok(())
    }

    fn remove_epitem(
        &self,
        epitem: &Arc<EPollItem>,
        private_data: &FilePrivateData,
    ) -> Result<(), SystemError> {
        let FilePrivateData::Pid(pid_data) = private_data else {
            return Err(SystemError::EBADF);
        };
        pid_data.pid().remove_pidfd_epitem(epitem)
    }
}

#[derive(Debug, Clone)]
pub struct PidFdTarget {
    pid: Arc<Pid>,
    flags: FileFlags,
}

impl PidFdTarget {
    fn new(pid: Arc<Pid>, flags: FileFlags) -> Self {
        Self { pid, flags }
    }

    pub fn pid(&self) -> Arc<Pid> {
        self.pid.clone()
    }

    pub fn flags(&self) -> FileFlags {
        self.flags
    }

    pub fn is_nonblock(&self) -> bool {
        self.flags.contains(FileFlags::O_NONBLOCK)
    }

    pub fn task(&self, pid_type: PidType) -> Option<Arc<ProcessControlBlock>> {
        self.pid.pid_task(pid_type)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidFdFileError {
    NotPidFd,
}

impl File {
    pub(crate) fn try_pidfd_target(&self) -> Result<PidFdTarget, PidFdFileError> {
        let pid = {
            let pdata = self.private_data.lock();
            match &*pdata {
                FilePrivateData::Pid(data) => data.pid(),
                _ => return Err(PidFdFileError::NotPidFd),
            }
        };

        Ok(PidFdTarget::new(pid, self.flags()))
    }

    pub(crate) fn pidfd_target(&self) -> Result<PidFdTarget, SystemError> {
        self.try_pidfd_target()
            .map_err(|PidFdFileError::NotPidFd| SystemError::EBADF)
    }
}

impl ProcessControlBlock {
    pub fn pidfd_target_from_fd(&self, fd: i32) -> Result<PidFdTarget, SystemError> {
        let file = self
            .fd_table()
            .read()
            .get_file_by_fd(fd)
            .ok_or(SystemError::EBADF)?;
        file.pidfd_target()
    }
}

pub struct PreparedPidFd {
    pub reservation: ReservedFd,
    pub file: File,
}

pub struct PidFd;

impl PidFd {
    const PREPARE_ALLOWED_FLAGS: FileFlags = FileFlags::O_NONBLOCK;

    pub fn create_file(pid: Arc<Pid>, flags: FileFlags) -> Result<File, SystemError> {
        let file = File::new_with_private_data(
            Arc::new(PidFdInode::new()),
            FileFlags::O_RDWR | flags,
            FilePrivateData::Pid(PidPrivateData::new(pid)),
        )?;
        Ok(file)
    }

    pub fn prepare(
        task: &Arc<ProcessControlBlock>,
        pid: Arc<Pid>,
        flags: FileFlags,
        require_tgid: bool,
    ) -> Result<PreparedPidFd, SystemError> {
        if flags.intersects(!Self::PREPARE_ALLOWED_FLAGS) {
            return Err(SystemError::EINVAL);
        }
        if require_tgid && pid.pid_task(PidType::TGID).is_none() {
            return Err(SystemError::EINVAL);
        }

        let reservation = task.fd_table().write().reserve_fd(true)?;
        let file = match Self::create_file(pid, flags) {
            Ok(file) => file,
            Err(err) => {
                task.fd_table().write().release_reserved_fd(reservation);
                return Err(err);
            }
        };

        Ok(PreparedPidFd { reservation, file })
    }
}
