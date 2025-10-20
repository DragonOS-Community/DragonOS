use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use crate::filesystem::vfs::file::FileMode;
use crate::filesystem::vfs::file::FilePrivateData;
use crate::filesystem::vfs::file_operations::FileOperations;
use crate::filesystem::vfs::{FileType, IndexNode, InodeId, Metadata};
use crate::process::pid::PidPrivateData;
use crate::{
    driver::base::block::SeekFrom,
    filesystem::{epoll::EPollItem, vfs::FilldirContext},
    libs::{rwlock::RwLock, spinlock::SpinLock},
    process::{cred::Cred, ProcessManager},
};

/// @brief 抽象文件结构体
#[derive(Debug)]
pub struct Pidfd {
    /// inode, 这里不会使用 inode 相关的操作, 这都是没有意义的, 他只做基础的一些事情
    /// 比如引用计数, 自动调用 release 函数等
    /// TODO: 实现相关的操作
    //inode: Arc<dyn IndexNode>,
    /// 文件的打开模式
    mode: RwLock<FileMode>,
    /// 文件类型
    file_type: FileType,
    /// 私有的部分数据
    pub private_data: SpinLock<FilePrivateData>,
    /// 文件的凭证
    cred: Cred,
}

impl Pidfd {
    pub fn new(pid: i32, mode: FileMode, file_type: FileType) -> Result<Self, SystemError> {
        let f = Self {
            //inode,
            mode: RwLock::new(mode),
            file_type,
            // 这里应该使用 find_get_pid 等函数将 i32 类型的 pid 获取其 struct pid
            // 见pid.rs中的详细说明, 这里直接创建了
            private_data: SpinLock::new(FilePrivateData::Pid(PidPrivateData::new(pid))),
            cred: (*ProcessManager::current_pcb().cred()).clone(),
        };

        return Ok(f);
    }

    fn mode(&self) -> FileMode {
        *self.mode.read()
    }
}

impl FileOperations for Pidfd {
    fn read(&self, _len: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        log::error!("Pidfd: read!");
        Err(SystemError::ENOSYS)
    }

    fn write(&self, _len: usize, _buf: &[u8]) -> Result<usize, SystemError> {
        log::error!("Pidfd: write!");
        Err(SystemError::ENOSYS)
    }

    fn pread(&self, _offset: usize, _len: usize, _buf: &mut [u8]) -> Result<usize, SystemError> {
        log::error!("Pidfd: pread!");
        Err(SystemError::ENOSYS)
    }

    fn pwrite(&self, _offset: usize, _len: usize, _buf: &[u8]) -> Result<usize, SystemError> {
        log::error!("Pidfd: pwrite!");
        Err(SystemError::ENOSYS)
    }

    fn lseek(&self, _origin: SeekFrom) -> Result<usize, SystemError> {
        log::error!("Pidfd: lseek!");
        Err(SystemError::ENOSYS)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        log::error!("Pidfd: metadata!");
        Err(SystemError::ENOSYS)
    }

    fn get_entry_name(&self, _ino: InodeId) -> Result<String, SystemError> {
        log::error!("Pidfd: get entry name!");
        Err(SystemError::ENOSYS)
    }

    fn read_dir(&self, _ctx: &mut FilldirContext) -> Result<(), SystemError> {
        log::error!("Pidfd: read_dir!");
        Err(SystemError::ENOSYS)
    }

    fn readable(&self) -> Result<(), SystemError> {
        log::error!("Pidfd: readable!");
        Err(SystemError::ENOSYS)
    }

    fn writeable(&self) -> Result<(), SystemError> {
        log::error!("Pidfd: writeable!");
        Err(SystemError::ENOSYS)
    }

    fn close_on_exec(&self) -> bool {
        self.mode().contains(FileMode::O_CLOEXEC)
    }

    fn set_close_on_exec(&self, close_on_exec: bool) {
        let mut mode_guard = self.mode.write();
        if close_on_exec {
            mode_guard.insert(FileMode::O_CLOEXEC);
        } else {
            mode_guard.remove(FileMode::O_CLOEXEC);
        }
    }

    fn ftruncate(&self, _len: usize) -> Result<(), SystemError> {
        log::error!("Pidfd: ftruncate!");
        Err(SystemError::ENOSYS)
    }

    fn add_epitem(&self, _epitem: Arc<EPollItem>) -> Result<(), SystemError> {
        log::error!("Pidfd: add_epitem!");
        Err(SystemError::ENOSYS)
    }

    fn remove_epitem(&self, _epitem: &Arc<EPollItem>) -> Result<(), SystemError> {
        log::error!("Pidfd: remove_epitem!");
        Err(SystemError::ENOSYS)
    }

    fn poll(&self) -> Result<usize, SystemError> {
        log::error!("Pidfd: poll!");
        Err(SystemError::ENOSYS)
    }

    fn file_type(&self) -> FileType {
        self.file_type
    }

    fn mode(&self) -> FileMode {
        *self.mode.read()
    }

    fn set_mode(&self, mode: FileMode) -> Result<(), SystemError> {
        *self.mode.write() = mode;
        self.private_data.lock().update_mode(mode);
        Ok(())
    }

    fn try_clone(&self) -> Option<Arc<dyn FileOperations>> {
        let cloned_pidfd = Pidfd {
            mode: RwLock::new(self.mode()),
            file_type: self.file_type,
            // 这里应该使用 find_get_pid 等函数将 i32 类型的 pid 获取其 struct pid
            // 见pid.rs中的详细说明, 这里直接创建了
            private_data: SpinLock::new(self.private_data.lock().clone()),
            cred: self.cred.clone(),
        };

        Some(Arc::new(cloned_pidfd))
    }

    fn inode(&self) -> Arc<dyn IndexNode> {
        panic!("Pidfd: inode!");
    }

    fn offset(&self) -> usize {
        log::error!("Pidfd: offset!");
        0
    }

    fn set_offset(&self, _offset: usize) {
        log::error!("Pidfd: set_offset!");
    }
}
