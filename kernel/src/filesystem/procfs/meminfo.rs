//! /proc/meminfo - 系统内存信息
//!
//! 这个文件展示了系统的内存使用情况

use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{syscall::ModeType, FilePrivateData, IndexNode},
    },
    mm::allocator::page_frame::FrameAllocator,
};
use alloc::{
    borrow::ToOwned,
    format,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

/// /proc/meminfo 文件的 FileOps 实现
#[derive(Debug)]
pub struct MeminfoFileOps;

impl MeminfoFileOps {
    pub fn new_inode(parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self, ModeType::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_meminfo_content() -> Vec<u8> {
        use crate::arch::mm::LockedFrameAllocator;

        let usage = unsafe { LockedFrameAllocator.usage() };

        let mut data: Vec<u8> = vec![];

        data.append(
            &mut format!("MemTotal:\t{} kB\n", usage.total().bytes() >> 10)
                .as_bytes()
                .to_owned(),
        );

        data.append(
            &mut format!("MemFree:\t{} kB\n", usage.free().bytes() >> 10)
                .as_bytes()
                .to_owned(),
        );

        // 去除多余的 \0 并在结尾添加 \0
        trim_string(&mut data);

        data
    }
}

impl FileOps for MeminfoFileOps {
    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        _data: crate::libs::spinlock::SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_meminfo_content();
        proc_read(offset, len, buf, &content)
    }
}
