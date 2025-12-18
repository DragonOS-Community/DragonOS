use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        procfs::template::Common,
        vfs::{
            file::FileMode, syscall::ModeType, vcore::generate_inode_id, FilePrivateData,
            FileSystem, FileType, IndexNode, Metadata,
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

/// ProcSym 是 procfs 符号链接的泛型包装器
/// S 是实现了 SymOps trait 的具体符号链接操作类型
#[derive(Debug)]
pub struct ProcSym<S: SymOps> {
    inner: S,
    self_ref: Weak<ProcSym<S>>,
    parent: Option<Weak<dyn IndexNode>>,
    common: Common,
}

impl<S: SymOps> ProcSym<S> {
    /// 创建一个新的 ProcSym
    pub(super) fn new(
        sym: S,
        fs: Weak<dyn FileSystem>,
        parent: Option<Weak<dyn IndexNode>>,
        is_volatile: bool,
        mode: ModeType,
    ) -> Arc<Self> {
        Self::new_with_data(sym, fs, parent, is_volatile, mode, 0)
    }

    /// 创建一个新的 ProcSym（带额外数据）
    pub(super) fn new_with_data(
        sym: S,
        fs: Weak<dyn FileSystem>,
        parent: Option<Weak<dyn IndexNode>>,
        is_volatile: bool,
        mode: ModeType,
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
                file_type: FileType::SymLink,
                mode,
                nlinks: 1,
                uid: 0,
                gid: 0,
                raw_dev: DeviceNumber::from(data as u32),
            };
            Common::new(metadata, fs, is_volatile)
        };

        Arc::new_cyclic(|weak_self| Self {
            inner: sym,
            self_ref: weak_self.clone(),
            parent,
            common,
        })
    }

    pub fn self_ref(&self) -> Option<Arc<ProcSym<S>>> {
        self.self_ref.upgrade()
    }

    pub fn self_ref_weak(&self) -> &Weak<ProcSym<S>> {
        &self.self_ref
    }

    pub fn parent(&self) -> Option<Arc<dyn IndexNode>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }
}

/// SymOps trait 定义了 procfs 符号链接需要实现的操作
pub trait SymOps: Sync + Send + Sized + Debug {
    /// 读取符号链接的目标路径
    ///
    /// # 返回值
    /// - `Ok(String)`: 符号链接指向的目标路径
    /// - `Err(SystemError)`: 错误码
    fn read_link(&self, buf: &mut [u8]) -> Result<usize, SystemError>;
}

/// 为 ProcSym 实现 IndexNode trait
/// 使用 inherit_methods 宏从 common 继承通用方法
#[inherit_methods(from = "self.common")]
impl<S: SymOps + 'static> IndexNode for ProcSym<S> {
    fn fs(&self) -> Arc<dyn FileSystem>;
    fn as_any_ref(&self) -> &dyn core::any::Any;
    fn metadata(&self) -> Result<Metadata, SystemError>;
    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError>;

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        //todo 符号链接不能直接读取，但是由于目前系统中对于 readlink 的支持有限
        //暂时通过 read_at 来模拟 readlink 的行为
        // log::info!("ProcSym read_at called, redirecting to read_link");
        self.inner.read_link(buf)
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        _buf: &[u8],
        _data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        // 符号链接不能写入
        Err(SystemError::EINVAL)
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
        _mode: &FileMode,
    ) -> Result<(), SystemError> {
        return Ok(());
    }

    fn close(&self, _data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }
}
