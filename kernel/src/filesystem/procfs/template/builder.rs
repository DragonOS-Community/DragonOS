use crate::filesystem::{
    procfs::template::{DirOps, FileOps, ProcDir, ProcFile, ProcSym, SymOps},
    vfs::{syscall::ModeType, FileSystem, IndexNode},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

struct BuilderCommon {
    mode: ModeType,
    parent: Option<Weak<dyn IndexNode>>,
    fs: Option<Weak<dyn FileSystem>>,
    is_volatile: bool,
    data: usize,
}

impl BuilderCommon {
    fn new(mode: ModeType) -> Self {
        Self {
            mode,
            parent: None,
            fs: None,
            is_volatile: false,
            data: 0,
        }
    }

    fn set_parent(&mut self, parent: Weak<dyn IndexNode>) {
        self.parent = Some(parent);
    }

    fn set_fs(&mut self, fs: Weak<dyn FileSystem>) {
        self.fs = Some(fs);
    }

    fn set_volatile(&mut self) {
        self.is_volatile = true;
    }
}

pub trait Builder<Ops> {
    type Output;

    fn new(ops: Ops, mode: ModeType) -> Self;
    fn build(self) -> Result<Arc<Self::Output>, SystemError>;
}

pub struct ProcFileBuilder<F: FileOps> {
    file: F,
    common: BuilderCommon,
}

impl<F: FileOps> ProcFileBuilder<F> {
    pub fn parent(mut self, parent: Weak<dyn IndexNode>) -> Self {
        self.common.set_parent(parent);
        self
    }
}

impl<F> Builder<F> for ProcFileBuilder<F>
where
    F: FileOps,
{
    type Output = ProcFile<F>;

    /// 创建一个新的 ProcFileBuilder
    ///
    /// # 参数
    /// - `file`: 实现了 FileOps trait 的文件操作对象
    /// - `mode`: 文件权限模式
    fn new(file: F, mode: ModeType) -> Self {
        Self {
            file,
            common: BuilderCommon::new(mode),
        }
    }

    fn build(self) -> Result<Arc<Self::Output>, SystemError> {
        // 从父节点获取文件系统（如果未显式设置）
        let fs = if let Some(fs) = self.common.fs {
            fs
        } else if let Some(parent) = &self.common.parent {
            if let Some(parent_node) = parent.upgrade() {
                Arc::downgrade(&parent_node.fs())
            } else {
                return Err(SystemError::EINVAL);
            }
        } else {
            return Err(SystemError::EINVAL);
        };

        Ok(ProcFile::new_with_data(
            self.file,
            fs,
            self.common.parent,
            self.common.is_volatile,
            self.common.mode,
            self.common.data,
        ))
    }
}

pub struct ProcDirBuilder<D: DirOps> {
    dir: D,
    common: BuilderCommon,
}

impl<D: DirOps> ProcDirBuilder<D> {
    pub fn parent(mut self, parent: Weak<dyn IndexNode>) -> Self {
        self.common.set_parent(parent);
        self
    }

    pub fn fs(mut self, fs: Weak<dyn FileSystem>) -> Self {
        self.common.set_fs(fs);
        self
    }

    pub fn volatile(mut self) -> Self {
        self.common.set_volatile();
        self
    }
}

impl<D> Builder<D> for ProcDirBuilder<D>
where
    D: DirOps,
{
    type Output = ProcDir<D>;

    /// 创建一个新的 ProcDirBuilder
    ///
    /// # 参数
    /// - `dir`: 实现了 DirOps trait 的目录操作对象
    /// - `mode`: 目录权限模式
    fn new(dir: D, mode: ModeType) -> Self {
        Self {
            dir,
            common: BuilderCommon::new(mode),
        }
    }

    fn build(self) -> Result<Arc<Self::Output>, SystemError> {
        // 从父节点获取文件系统（如果未显式设置）
        let fs = if let Some(fs) = self.common.fs {
            fs
        } else if let Some(parent) = &self.common.parent {
            if let Some(parent_node) = parent.upgrade() {
                Arc::downgrade(&parent_node.fs())
            } else {
                return Err(SystemError::EINVAL);
            }
        } else {
            return Err(SystemError::EINVAL);
        };

        Ok(ProcDir::new_with_data(
            self.dir,
            fs,
            self.common.parent,
            self.common.is_volatile,
            self.common.mode,
            self.common.data,
        ))
    }
}

pub struct ProcSymBuilder<S: SymOps> {
    sym: S,
    common: BuilderCommon,
}

impl<S: SymOps> ProcSymBuilder<S> {
    pub fn parent(mut self, parent: Weak<dyn IndexNode>) -> Self {
        self.common.set_parent(parent);
        self
    }

    pub fn volatile(mut self) -> Self {
        self.common.set_volatile();
        self
    }
}

impl<S> Builder<S> for ProcSymBuilder<S>
where
    S: SymOps,
{
    type Output = ProcSym<S>;

    /// 创建一个新的 ProcSymBuilder
    ///
    /// # 参数
    /// - `sym`: 实现了 SymOps trait 的符号链接操作对象
    /// - `mode`: 符号链接权限模式
    fn new(sym: S, mode: ModeType) -> Self {
        Self {
            sym,
            common: BuilderCommon::new(mode),
        }
    }

    fn build(self) -> Result<Arc<ProcSym<S>>, SystemError> {
        // 从父节点获取文件系统（如果未显式设置）
        let fs = if let Some(fs) = self.common.fs {
            fs
        } else if let Some(parent) = &self.common.parent {
            if let Some(parent_node) = parent.upgrade() {
                Arc::downgrade(&parent_node.fs())
            } else {
                return Err(SystemError::EINVAL);
            }
        } else {
            return Err(SystemError::EINVAL);
        };

        Ok(ProcSym::new_with_data(
            self.sym,
            fs,
            self.common.parent,
            self.common.is_volatile,
            self.common.mode,
            self.common.data,
        ))
    }
}
