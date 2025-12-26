//! ProcFS Template 系统
//!
//! 这个模块提供了一套基于 trait 和泛型的 template 系统，用于简化 procfs 的实现。
//! 设计参考了 Asterinas 的 procfs template 系统。
//!
//! # 核心组件
//!
//! - `Common`: 共享的元数据和行为
//! - `ProcFile<F>`: 文件的泛型包装器
//! - `ProcDir<D>`: 目录的泛型包装器
//! - `ProcSym<S>`: 符号链接的泛型包装器
//! - `FileOps`, `DirOps`, `SymOps`: 定义定制点的 trait
//! - Builder 模式：用于灵活构造 inode

use crate::{
    filesystem::vfs::{utils::DName, FileSystem, Metadata},
    libs::rwlock::RwLock,
};

use alloc::sync::{Arc, Weak};
use system_error::SystemError;

mod builder;
mod dir;
mod file;
mod sym;

// 公开导出
pub use self::{
    builder::{Builder, ProcDirBuilder, ProcFileBuilder, ProcSymBuilder},
    dir::{lookup_child_from_table, populate_children_from_table, DirOps, ProcDir},
    file::{FileOps, ProcFile},
    sym::{ProcSym, SymOps},
};

/// Common - 所有 procfs inode 共享的基础设施
///
/// 包含：
/// - 元数据（inode 号、权限、所有者、时间戳等）
/// - 文件系统引用
#[derive(Debug)]
pub(super) struct Common {
    fs: Weak<dyn FileSystem>,
    metadata: RwLock<Metadata>,
    pub(super) dname: DName,
}

impl Common {
    /// 创建一个新的 Common 实例
    pub fn new(metadata: Metadata, fs: Weak<dyn FileSystem>, _is_volatile: bool) -> Self {
        Self {
            metadata: RwLock::new(metadata),
            fs,
            dname: DName::default(),
        }
    }

    /// 获取文件系统引用
    pub(super) fn fs(&self) -> Arc<dyn FileSystem> {
        self.fs.upgrade().unwrap()
    }

    /// 获取 Any 引用（用于类型转换）
    pub(super) fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }

    /// 获取元数据
    pub(super) fn metadata(&self) -> Result<Metadata, SystemError> {
        let metadata = self.metadata.read().clone();
        Ok(metadata)
    }

    /// 设置元数据
    pub(super) fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        let mut meta = self.metadata.write();
        meta.atime = metadata.atime;
        meta.mtime = metadata.mtime;
        meta.ctime = metadata.ctime;
        meta.btime = metadata.btime;
        meta.mode = metadata.mode;
        meta.uid = metadata.uid;
        meta.gid = metadata.gid;

        Ok(())
    }
}
