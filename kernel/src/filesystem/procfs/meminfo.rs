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
    format,
    string::String,
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

    let mem_total_kb = (usage.total().bytes() >> 10) as u64;
	let mem_free_kb = (usage.free().bytes() >> 10) as u64;

	let cached_pages = stats.file_pages.saturating_sub(stats.shmem_pages);
	let cached_kb = cached_pages * page_kb;

	let dirty_kb = stats.file_dirty * page_kb;
	let writeback_kb = stats.file_writeback * page_kb;
	let mapped_kb = stats.file_mapped * page_kb;
	let shmem_kb = stats.shmem_pages * page_kb;

	let mem_available_kb = mem_free_kb.saturating_add(cached_kb);

    let mut s = String::new();

    macro_rules! push_kb {
        ($name:expr, $value:expr) => {
            s.push_str(&format!("{:<15}{:>8} kB\n", concat!($name, ":"), $value));
        };
    }

    push_kb!("MemTotal", mem_total_kb);
    push_kb!("MemFree", mem_free_kb);
    push_kb!("MemAvailable", mem_available_kb);
    push_kb!("Buffers", 0u64);
    push_kb!("Cached", cached_kb);
    push_kb!("SwapTotal", 0u64);
    push_kb!("SwapFree", 0u64);
    push_kb!("Dirty", dirty_kb);
    push_kb!("Writeback", writeback_kb);
    push_kb!("Mapped", mapped_kb);
    push_kb!("Shmem", shmem_kb);
    push_kb!("Slab", 0u64);
    push_kb!("SReclaimable", 0u64);
    push_kb!("SUnreclaim", 0u64);

    let mut data = s.into_bytes();
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
