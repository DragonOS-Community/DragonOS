//! /proc/sys/kernel - 内核参数目录
//!
//! 提供内核参数配置接口

use crate::libs::mutex::MutexGuard;
use crate::{
    debug::klog::loglevel::KERNEL_LOG_LEVEL,
    filesystem::{
        procfs::{
            template::{Builder, DirOps, FileOps, ProcDir, ProcDirBuilder, ProcFileBuilder},
            utils::proc_read,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
};
use alloc::{
    format,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/sys/kernel 目录的 DirOps 实现
#[derive(Debug)]
pub struct KernelDirOps;

impl KernelDirOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcDirBuilder::new(Self, InodeMode::from_bits_truncate(0o555))
            .parent(parent)
            .build()
            .unwrap()
    }
}

impl DirOps for KernelDirOps {
    fn lookup_child(
        &self,
        dir: &ProcDir<Self>,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name == "printk" {
            let mut cached_children = dir.cached_children().write();
            if let Some(child) = cached_children.get(name) {
                return Ok(child.clone());
            }

            let inode = PrintkFileOps::new_inode(dir.self_ref_weak().clone());
            cached_children.insert(name.to_string(), inode.clone());
            return Ok(inode);
        }

        Err(SystemError::ENOENT)
    }

    fn populate_children(&self, dir: &ProcDir<Self>) {
        let mut cached_children = dir.cached_children().write();
        cached_children
            .entry("printk".to_string())
            .or_insert_with(|| PrintkFileOps::new_inode(dir.self_ref_weak().clone()));
    }
}

/// /proc/sys/kernel/printk 文件的 FileOps 实现
#[derive(Debug)]
pub struct PrintkFileOps;

impl PrintkFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, InodeMode::from_bits_truncate(0o644))
            .parent(parent)
            .build()
            .unwrap()
    }

    /// 读取当前的内核日志级别配置
    fn read_config() -> String {
        let levels = KERNEL_LOG_LEVEL.get_all_levels();
        format!(
            "{}\t{}\t{}\t{}\n",
            levels[0], levels[1], levels[2], levels[3]
        )
    }

    /// 写入内核日志级别配置
    fn write_config(data: &[u8]) -> Result<usize, SystemError> {
        let input = core::str::from_utf8(data).map_err(|_| SystemError::EINVAL)?;
        let parts: Vec<&str> = input.split_whitespace().collect();

        if parts.is_empty() {
            return Err(SystemError::EINVAL);
        }

        // 只处理第一个值（控制台日志级别）
        if let Ok(level) = parts[0].parse::<u8>() {
            KERNEL_LOG_LEVEL.set_console_level(level)?;
            log::info!(
                "sysctl: set console log level to {} via /proc/sys/kernel/printk",
                level
            );
            Ok(data.len())
        } else {
            log::warn!("sysctl: invalid loglevel value '{}'", parts[0]);
            Err(SystemError::EINVAL)
        }
    }
}

impl FileOps for PrintkFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::read_config();
        proc_read(offset, len, buf, content.as_bytes())
    }

    fn write_at(
        &self,
        _offset: usize,
        _len: usize,
        buf: &[u8],
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        Self::write_config(buf)
    }
}
