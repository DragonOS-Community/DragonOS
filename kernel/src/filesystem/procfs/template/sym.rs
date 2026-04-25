use crate::libs::mutex::MutexGuard;
use crate::{
    driver::base::device::device_number::DeviceNumber,
    filesystem::{
        procfs::template::Common,
        vfs::{
            file::FileFlags, vcore::generate_inode_id, FilePrivateData, FileSystem, FileType,
            IndexNode, InodeFlags, InodeId, InodeMode, Metadata, SpecialNodeData,
        },
    },
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
    common: Common,
}

impl<S: SymOps + 'static> ProcSym<S> {
    /// 创建一个新的 ProcSym（带额外数据）
    pub(super) fn new_with_data(
        sym: S,
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
                file_type: FileType::SymLink,
                mode,
                flags: InodeFlags::empty(),
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
            common,
        })
    }

    /// 获取自引用（用于 special_node）
    fn self_arc(&self) -> Option<Arc<dyn IndexNode>> {
        self.self_ref.upgrade().map(|arc| arc as Arc<dyn IndexNode>)
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

    /// 返回特殊节点数据（用于"魔法链接"如 /proc/self/fd/N）
    ///
    /// 魔法链接是一种特殊的符号链接，它的 readlink 返回的路径可能不可解析（如 pipe:[xxx]），
    /// 但通过 special_node 可以直接获取目标 inode 的引用，使得 open/stat 等操作可以正常工作。
    ///
    /// 默认实现返回 None，表示这是一个普通符号链接。
    /// 对于 /proc/self/fd/N，返回文件的 inode。
    /// 对于 /proc/*/ns/*，应返回 None 并实现 is_self_reference()。
    fn special_node(&self) -> Option<SpecialNodeData> {
        None
    }

    /// 是否是自引用的魔法链接（如 /proc/*/ns/* 命名空间文件）
    ///
    /// 这类符号链接的 readlink 返回不可解析的路径（如 ipc:[xxx]），
    /// 但可以直接打开并操作（如用于 setns()）。
    /// 当返回 true 时，ProcSym 会在 special_node() 中返回自身的引用。
    fn is_self_reference(&self) -> bool {
        false
    }

    /// 打开符号链接时调用，可用于设置命名空间文件私有数据
    ///
    /// 用于 /proc/*/ns/* 这类可以直接打开的"魔法链接"
    fn open(&self, _data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        // 默认实现：不做任何操作
        Ok(())
    }

    /// 返回动态 inode ID（用于命名空间文件）
    ///
    /// 命名空间文件的 inode ID 应该是命名空间的 ID，而不是 procfs 分配的固定 ID。
    /// 这样 stat() 返回的 st_ino 就是命名空间 ID，可以用于比较两个命名空间是否相同。
    ///
    /// 默认返回 None，表示使用 procfs 分配的固定 inode ID。
    fn dynamic_inode_id(&self) -> Option<InodeId> {
        None
    }

    /// 返回动态 owner（用于 /proc/<pid> 等需要实时 UID/GID 的场景）
    fn owner(&self) -> Option<(usize, usize)> {
        None
    }
}

/// 为 ProcSym 实现 IndexNode trait
/// 使用 inherit_methods 宏从 common 继承通用方法
#[inherit_methods(from = "self.common")]
impl<S: SymOps + 'static> IndexNode for ProcSym<S> {
    fn fs(&self) -> Arc<dyn FileSystem>;
    fn as_any_ref(&self) -> &dyn core::any::Any;
    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError>;

    fn metadata(&self) -> Result<Metadata, SystemError> {
        let mut metadata = self.common.metadata()?;

        if let Some((uid, gid)) = self.inner.owner() {
            metadata.uid = uid;
            metadata.gid = gid;
        }

        // 如果 inner 提供了动态 inode ID（如命名空间文件），使用它
        if let Some(dynamic_id) = self.inner.dynamic_inode_id() {
            metadata.inode_id = dynamic_id;
        }

        Ok(metadata)
    }

    fn read_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
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
        _data: MutexGuard<FilePrivateData>,
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
        mut data: MutexGuard<FilePrivateData>,
        _flags: &FileFlags,
    ) -> Result<(), SystemError> {
        self.inner.open(&mut data)
    }

    fn close(&self, _data: MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        return Ok(());
    }

    fn special_node(&self) -> Option<SpecialNodeData> {
        // 首先检查 inner 是否提供了 special_node
        if let Some(data) = self.inner.special_node() {
            return Some(data);
        }

        // 如果 inner 是自引用的魔法链接（如 /proc/*/ns/*），返回自身引用
        if self.inner.is_self_reference() {
            return self.self_arc().map(SpecialNodeData::Reference);
        }

        None
    }
}
