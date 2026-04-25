use crate::filesystem::page_cache::PageCache;
use crate::mm::MemoryManagementArch;
use alloc::sync::Arc;

/// # 功能
///
/// 从指定页索引开始，截断与当前文件相关的所有页缓存页。
///
/// # 参数
///
/// - page_cache: 与文件inode关联的页缓存
/// - start: 起始页索引
pub fn truncate_inode_pages(page_cache: Arc<PageCache>, start: usize) {
    if let Err(err) = page_cache.truncate(start << crate::arch::MMArch::PAGE_SHIFT) {
        log::warn!("truncate_inode_pages failed: start_page={start}, err={err:?}");
    }
}
