use core::any::Any;
use core::fmt::Debug;

use alloc::{string::String, sync::Arc};
use system_error::SystemError;

use super::file::FileMode;
use super::{FileType, IndexNode, InodeId, Metadata};
use crate::{
    driver::base::block::SeekFrom,
    filesystem::{epoll::EPollItem, vfs::FilldirContext},
};

/// FileOperations trait - 类似于Linux的file_operations结构体
pub trait FileOperations: Send + Sync + Any + Debug {
    /// 从文件中读取指定的字节数到buffer中
    fn read(&self, len: usize, buf: &mut [u8]) -> Result<usize, SystemError>;

    /// 从buffer向文件写入指定的字节数的数据
    fn write(&self, len: usize, buf: &[u8]) -> Result<usize, SystemError>;

    /// 从文件中指定的偏移处读取指定的字节数到buf中
    fn pread(&self, offset: usize, len: usize, buf: &mut [u8]) -> Result<usize, SystemError>;

    /// 从buf向文件中指定的偏移处写入指定的字节数的数据
    fn pwrite(&self, offset: usize, len: usize, buf: &[u8]) -> Result<usize, SystemError>;

    /// 调整文件操作指针的位置
    fn lseek(&self, origin: SeekFrom) -> Result<usize, SystemError>;

    /// 获取文件的元数据
    fn metadata(&self) -> Result<Metadata, SystemError>;

    /// 根据inode号获取子目录项的名字
    fn get_entry_name(&self, ino: InodeId) -> Result<String, SystemError>;

    /// 读取目录项
    fn read_dir(&self, ctx: &mut FilldirContext) -> Result<(), SystemError>;

    /// 判断当前文件是否可读
    fn readable(&self) -> Result<(), SystemError>;

    /// 判断当前文件是否可写
    fn writeable(&self) -> Result<(), SystemError>;

    /// 获取文件是否在execve时关闭
    fn close_on_exec(&self) -> bool;

    /// 设置文件是否在execve时关闭
    fn set_close_on_exec(&self, close_on_exec: bool);

    /// 重新设置文件的大小
    fn ftruncate(&self, len: usize) -> Result<(), SystemError>;

    /// Add an EPollItem to the file
    fn add_epitem(&self, epitem: Arc<EPollItem>) -> Result<(), SystemError>;

    /// Remove epitems associated with the epoll
    fn remove_epitem(&self, epitem: &Arc<EPollItem>) -> Result<(), SystemError>;

    /// Poll the file for events
    fn poll(&self) -> Result<usize, SystemError>;

    /// 获取文件的类型
    fn file_type(&self) -> FileType;

    /// 获取文件的打开模式
    fn mode(&self) -> FileMode;

    /// 设置文件的打开模式
    fn set_mode(&self, mode: FileMode) -> Result<(), SystemError>;

    /// 尝试克隆一个文件
    fn try_clone(&self) -> Option<Arc<dyn FileOperations>>;

    /// 获取底层inode
    fn inode(&self) -> Arc<dyn IndexNode>;

    /// 获取当前文件偏移量
    fn offset(&self) -> usize;

    /// 设置文件偏移量
    fn set_offset(&self, offset: usize);
}

impl dyn FileOperations {
    fn as_any_arc(self: Arc<Self>) -> Arc<dyn Any> {
        return self;
    }

    pub fn downcast_arc<T: FileOperations>(self: Arc<Self>) -> Option<Arc<T>> {
        let x = self.as_any_arc();

        if x.is::<T>() {
            // into_raw不会改变引用计数
            let p = Arc::into_raw(x);
            let new = unsafe { Arc::from_raw(p as *const T) };
            return Some(new);
        }
        return None;
    }
}
