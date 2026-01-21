//! /proc/meminfo - 系统内存信息
//!
//! 这个文件展示了系统的内存使用情况

use crate::libs::mutex::MutexGuard;
use crate::mm::MemoryManagementArch;
use crate::{
    filesystem::{
        procfs::{
            template::{Builder, FileOps, ProcFileBuilder},
            utils::{proc_read, trim_string},
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    mm::allocator::page_frame::FrameAllocator,
    mm::page_cache_stats,
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
        ProcFileBuilder::new(Self, InodeMode::S_IRUGO) // 0444 - 所有用户可读
            .parent(parent)
            .build()
            .unwrap()
    }

    fn generate_meminfo_content() -> Vec<u8> {
        use crate::arch::mm::LockedFrameAllocator;
        use crate::arch::MMArch;

        let usage = unsafe { LockedFrameAllocator.usage() };
        let stats = page_cache_stats::snapshot();
        let page_kb = (MMArch::PAGE_SIZE >> 10) as u64;
        let cached_pages = stats.file_pages.saturating_sub(stats.shmem_pages);

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

        data.append(&mut format!("Buffers:\t{} kB\n", 0u64).as_bytes().to_owned());
        data.append(
            &mut format!("Cached:\t\t{} kB\n", cached_pages * page_kb)
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("Dirty:\t\t{} kB\n", stats.file_dirty * page_kb)
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("Writeback:\t{} kB\n", stats.file_writeback * page_kb)
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("Mapped:\t\t{} kB\n", stats.file_mapped * page_kb)
                .as_bytes()
                .to_owned(),
        );
        data.append(
            &mut format!("Shmem:\t\t{} kB\n", stats.shmem_pages * page_kb)
                .as_bytes()
                .to_owned(),
        );
        data.append(&mut format!("Slab:\t\t{} kB\n", 0u64).as_bytes().to_owned());
        data.append(
            &mut format!("SReclaimable:\t{} kB\n", 0u64)
                .as_bytes()
                .to_owned(),
        );
        data.append(&mut format!("SUnreclaim:\t{} kB\n", 0u64).as_bytes().to_owned());

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
        _data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        let content = Self::generate_meminfo_content();
        proc_read(offset, len, buf, &content)
    }
}
