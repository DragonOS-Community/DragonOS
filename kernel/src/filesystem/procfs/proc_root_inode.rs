use alloc::{string::String, sync::Arc, vec::Vec};
use system_error::SystemError;
use log::debug;

use crate::{
    filesystem::{
        kernfs::KernFSInode,
        kernfs::dynamic::DynamicLookup,
        vfs::{
            IndexNode, Metadata, FilePrivateData, 
            syscall::ModeType, FileType, file::FileMode
        },
    },
    libs::spinlock::SpinLockGuard,
};

use super::dynamic_pid_lookup::ProcFSDynamicPidLookup;

/// ProcFS 根 IndexNode 包装器
/// 重写 find 和 list 方法以支持动态 PID 目录查找
#[derive(Debug)]
pub struct ProcFSRootInode {
    kernfs_inode: Arc<KernFSInode>,
    dynamic_lookup: Arc<ProcFSDynamicPidLookup>,
}

impl ProcFSRootInode {
    pub fn new(kernfs_inode: Arc<KernFSInode>, dynamic_lookup: Arc<ProcFSDynamicPidLookup>) -> Arc<Self> {
        Arc::new(Self {
            kernfs_inode,
            dynamic_lookup,
        })
    }
}

impl IndexNode for ProcFSRootInode {
    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // debug!("ProcFSRootInode::find: Looking for '{}'", name);
        
        // 首先尝试静态查找
        match self.kernfs_inode.find(name) {
            Ok(inode) => return Ok(inode),
            Err(SystemError::ENOENT) => {
                // 如果静态查找失败，尝试动态查找
                match self.dynamic_lookup.dynamic_find(name)? {
                    Some(inode) => return Ok(inode),
                    None => {} // 继续返回 ENOENT
                }
                Err(SystemError::ENOENT)
            }
            Err(e) => Err(e),
        }
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        // debug!("ProcFSRootInode::list: Listing directory contents");
        
        // 获取KernFS的静态条目（系统文件和目录）
        let mut entries = self.kernfs_inode.list()?;
        
        // 使用动态查找获取当前存活的PID目录
        match self.dynamic_lookup.dynamic_list() {
            Ok(live_pids) => {
                // 过滤掉数字条目（PID目录），只保留非数字的静态条目
                entries.retain(|name| name.parse::<u32>().is_err());
                
                // 添加当前存活的PID目录
                for pid_str in live_pids {
                    entries.push(pid_str);
                }
                
                // 排序以确保稳定的输出顺序
                entries.sort();
                
                // debug!("ProcFSRootInode::list: Returning {} entries after dynamic filtering", entries.len());
            }
            Err(e) => {
                debug!("ProcFSRootInode::list: dynamic_list failed: {:?}, using static entries only", e);
            }
        }
        
        Ok(entries)
    }

    // 委托其他方法到 kernfs_inode
    fn open(
        &self,
        data: SpinLockGuard<FilePrivateData>,
        mode: &FileMode,
    ) -> Result<(), SystemError> {
        self.kernfs_inode.open(data, mode)
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.kernfs_inode.close(data)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.read_at(offset, len, buf, data)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.write_at(offset, len, buf, data)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.kernfs_inode.metadata()
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.kernfs_inode.set_metadata(metadata)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.kernfs_inode.resize(len)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.kernfs_inode.create_with_data(name, file_type, mode, data)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.kernfs_inode.link(name, other)
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.kernfs_inode.unlink(name)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.kernfs_inode.rmdir(name)
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        self.kernfs_inode.move_to(old_name, target, new_name)
    }

    fn get_entry_name(&self, ino: crate::filesystem::vfs::InodeId) -> Result<String, SystemError> {
        self.kernfs_inode.get_entry_name(ino)
    }

    fn get_entry_name_and_metadata(
        &self,
        ino: crate::filesystem::vfs::InodeId,
    ) -> Result<(String, Metadata), SystemError> {
        self.kernfs_inode.get_entry_name_and_metadata(ino)
    }

    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.ioctl(cmd, arg, private_data)
    }



    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.kernfs_inode.fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }








}