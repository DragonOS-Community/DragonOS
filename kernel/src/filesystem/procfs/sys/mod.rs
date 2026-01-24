//! /proc/sys - 系统控制目录
//!
//! 提供类似 Linux 的 /proc/sys 接口，支持动态配置内核参数

mod kernel;
mod vm;

use crate::filesystem::{
    procfs::template::{DirOps, ProcDir, ProcDirBuilder},
    vfs::{IndexNode, InodeMode},
};
use alloc::{
    string::ToString,
    sync::{Arc, Weak},
};
use kernel::KernelDirOps;
use system_error::SystemError;
use vm::VmDirOps;

use super::Builder;

/// /proc/sys 目录的 DirOps 实现
#[derive(Debug)]
pub struct SysDirOps;

impl SysDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for SysDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name == "kernel" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = KernelDirOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }
        if name == "vm" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = VmDirOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("kernel".to_string())
            .or_insert_with(|| KernelDirOps::new_inode(dir.self_ref_weak().clone()));
        cached_children
            .entry("vm".to_string())
            .or_insert_with(|| VmDirOps::new_inode(dir.self_ref_weak().clone()));
    }
}
