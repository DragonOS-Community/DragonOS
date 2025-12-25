use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        procfs::template::Common,
        vfs::{
            file::FileFlags, vcore::generate_inode_id, FilePrivateData, FileSystem, FileType,
            IndexNode, InodeFlags, InodeMode, Metadata,
        },
    },
    libs::spinlock::SpinLockGuard,
    time::PosixTimeSpec,
};
use alloc::fmt::Debug;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use inherit_methods_macro::inherit_methods;
use system_error::SystemError;

/// ProcFile 是 procfs 文件的泛型包装器
/// F 是实现了 FileOps trait 的具体文件操作类型
#[derive(Debug)]
pub struct ProcFile<F: FileOps> {
    inner: F,
    common: Common,
}

impl<F: FileOps> ProcFile<F> {
    /// 创建一个新的 ProcFile（带额外数据）
    pub(super) fn new_with_data(
        file: F,
        fs: Weak<dyn FileSystem>,
        _parent: Option<Weak<dyn IndexNode>>,
        is_volatile: bool,
        mode: InodeMode,
        data: usize,
    ) -> Arc<Self> {
        let common = {
            let metadata = Metadata {
                dev_id: 0,
                inode_id: generate_inode_id(),
                size: 0,
                blk_size: 0,
                blocks: 0,
                atime: PosixTimeSpec::default(),
                mtime: PosixTimeSpec::default(),
                ctime: PosixTimeSpec::default(),
                btime: PosixTimeSpec::default(),
                file_type: FileType::File,
                mode,
                flags: InodeFlags::empty(),
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data as u32),
            };
            Common::new(metadata, fs, is_volatile)
        };

        Arc::new(Self {
            inner: file,
            common,
        })
    }
}

/// FileOps trait 定义了 procfs 文件需要实现的操作
pub trait FileOps: Sync + Send + Sized + Debug {
    /// 从文件的指定偏移量读取数据
    ///
    /// # 参数
    /// - `offset`: 读取的起始偏移量
    /// - `len`: 要读取的字节数
    /// - `buf`: 存放读取数据的缓冲区
    /// - `data`: 文件私有数据
    ///
    /// # 返回值
    /// - `Ok(usize)`: 实际读取的字节数
    /// - `Err(SystemError)`: 错误码
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError>;

    /// 向文件的指定偏移量写入数据（可选，默认返回 EPERM）
    ///
    /// # 参数
    /// - `offset`: 写入的起始偏移量
    /// - `len`: 要写入的字节数
    /// - `buf`: 包含要写入数据的缓冲区
    /// - `data`: 文件私有数据
    ///
    /// # 返回值
    /// - `Ok(usize)`: 实际写入的字节数
    /// - `Err(SystemError)`: 错误码
    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }
}

/// 为 ProcFile 实现 IndexNode trait
/// 使用 inherit_methods 宏从 common 继承通用方法
#[inherit_methods(from = "self.common")]
impl<F: FileOps + 'static> IndexNode for ProcFile<F> {
    fn fs(&self) -> Arc<dyn FileSystem>;
    fn as_any_ref(&self) -> &dyn core::any::Any;
    fn metadata(&self) -> Result<Metadata, SystemError>;
    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError>;

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // log::info!("ProcFile read_at called");
        self.inner.read_at(offset, len, buf, data)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.inner.write_at(offset, len, buf, data)
    }

    fn list(&self) -> Result<Vec<String>, SystemError> {
        Err(SystemError::ENOTDIR)
    }

    fn find(&self, _name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        Err(SystemError::ENOTDIR)
    }

    fn open(
        &self,
        _data: SpinLockGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }
}
