//! /proc/net - 网络相关的 procfs 视图
//!
//! 目标：对齐 Linux /proc/net 的访问语义（目录 + 多个只读文件），并提供可扩展的结构，
//! 便于后续添加如 /proc/net/dev, /proc/net/route, /proc/net/tcp 等条目。

use crate::filesystem::{
    procfs::{
        template::{
            lookup_child_from_table, populate_children_from_table, DirOps, ProcDir, ProcDirBuilder,
        },
        Builder,
    },
    vfs::{IndexNode, InodeMode},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

mod arp;
mod protocols;

use arp::ArpFileOps;
use protocols::ProtocolsFileOps;

/// /proc/net 目录 DirOps
#[derive(Debug)]
pub struct NetDirOps;

impl NetDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }

    /// 静态条目表：/proc/net 下的文件/目录
    ///
    /// 设计上在这里集中注册，后续新增条目只需要在该模块添加对应 file/dir ops 并加入表。
    #[expect(clippy::type_complexity)]
    const STATIC_ENTRIES: &'static [(
        &'static str,
        fn(Weak<dyn IndexNode>) -> Arc<dyn IndexNode>,
    )] = &[
        ("arp", ArpFileOps::new_inode),
        ("protocols", ProtocolsFileOps::new_inode),
    ];
}

impl DirOps for NetDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let mut cached_children = dir.cached_children().write();

        if let Some(child) =
            lookup_child_from_table(name, &mut cached_children, Self::STATIC_ENTRIES, |f| {
                (f)(dir.self_ref_weak().clone())
            })
        {
            return Ok(child);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        populate_children_from_table(&mut cached_children, Self::STATIC_ENTRIES, |f| {
            (f)(dir.self_ref_weak().clone())
        });
    }
}
