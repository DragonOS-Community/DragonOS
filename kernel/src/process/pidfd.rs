use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::OpenFileBehavior;
use core::any::Any;

use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::filesystem::{
    anon_inode::{anon_inode_metadata, anon_inode_path, AnonInodeFs},
    epoll::{EPollEventType, EPollItem},
    vfs::{
        fdtable::FdReservation,
        file::{File, FileFlags, FilePrivateData, PreparedFile},
        FileSystem, IndexNode, InodeMode, Metadata, PollableInode,
    },
};
use crate::libs::mutex::MutexGuard;
use crate::mm::allocation::MemoryAllocationError;

use super::{
    pid::{Pid, PidPrivateData, PidType},
    ProcessControlBlock,
};

#[derive(Debug)]
pub struct PidFdInode {
    metadata: Metadata,
}

impl PidFdInode {
    fn new() -> Self {
        Self {
            metadata: anon_inode_metadata(InodeMode::S_IRUSR | InodeMode::S_IWUSR),
        }
    }
}

impl IndexNode for PidFdInode {
    fn configure_open_file(&self, _data: &FilePrivateData, behavior: &mut OpenFileBehavior) {
        behavior
            .mode
            .remove(FileMode::FMODE_CAN_READ | FileMode::FMODE_CAN_WRITE);
    }

    fn is_stream(&self) -> bool {
        true
    }

    fn open(
        &self,
        _data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Ok(());
        result.map_err(Into::into)
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
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Err(SystemError::EINVAL);
        result.map_err(Into::into)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, crate::mm::allocation::MemoryAllocationError> {
        let result: core::result::Result<_, system_error::SystemError> = Err(SystemError::EINVAL);
        result.map_err(Into::into)
    }

    fn fs(&self) -> Arc<dyn FileSystem> {
        AnonInodeFs::instance()
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

    fn stat_mode(&self, metadata: &Metadata) -> InodeMode {
        metadata.mode
    }

    fn absolute_path(&self) -> Result<String, SystemError> {
        Ok(anon_inode_path("[pidfd]"))
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
    pub reservation: FdReservation<1>,
    pub file: Arc<File>,
}

pub struct PidFd;

impl PidFd {
    const PREPARE_ALLOWED_FLAGS: FileFlags = FileFlags::O_NONBLOCK;

    pub(crate) fn create_file_prepared(
        pid: Arc<Pid>,
        flags: FileFlags,
        storage: PreparedFile,
    ) -> Result<File, MemoryAllocationError> {
        let file = Self::create_unbound_file_prepared(flags, storage)?;
        Self::bind_pid(&file, pid);
        Ok(file)
    }

    /// Complete all fallible File preparation while the object is private.
    /// The pidfd inode hooks do not inspect the target. Fork can therefore
    /// prepare this File before entering its namespace/publication barrier.
    pub(crate) fn create_unbound_file_prepared(
        flags: FileFlags,
        storage: PreparedFile,
    ) -> Result<File, MemoryAllocationError> {
        let inode = Arc::try_new(PidFdInode::new()).map_err(|_| SystemError::ENOMEM)?;
        File::new_pseudo_prepared(inode, FileFlags::O_RDWR | flags, storage)
    }

    /// Bind exactly once, before installing this private File into any table.
    /// This step cannot allocate or fail after the child PID has been born.
    pub(crate) fn bind_pid(file: &File, pid: Arc<Pid>) {
        let mut private_data = file.private_data.lock();
        debug_assert!(matches!(*private_data, FilePrivateData::Unused));
        *private_data = FilePrivateData::Pid(PidPrivateData::new(pid));
    }

    pub fn prepare(
        task: &Arc<ProcessControlBlock>,
        pid: Arc<Pid>,
        flags: FileFlags,
        require_tgid: bool,
    ) -> Result<PreparedPidFd, MemoryAllocationError> {
        if flags.intersects(!Self::PREPARE_ALLOWED_FLAGS) {
            return Err(SystemError::EINVAL.into());
        }
        if require_tgid && pid.pid_task(PidType::TGID).is_none() {
            return Err(SystemError::EINVAL.into());
        }

        let creator = crate::cgroup::memory::object_for_task(task);
        let reservation =
            task.fd_table()
                .reserve::<1>(task.nofile_soft_limit(), 0, true, &creator)?;
        let storage = PreparedFile::try_new_for(&creator)?;
        let file = Arc::try_new(Self::create_file_prepared(pid, flags, storage)?)
            .map_err(|_| SystemError::ENOMEM)?;

        Ok(PreparedPidFd { reservation, file })
    }
}
