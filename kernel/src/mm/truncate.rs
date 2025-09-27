use super::page::{page_reclaimer_lock_irqsave, Page, PageFlags};
use crate::filesystem::page_cache::PageCache;
use alloc::sync::Arc;

/// # 功能
///
/// 从指定偏移量开始，截断与当前文件的所有页缓存，目前仅是将文件相关的页缓存页的dirty位去除
///
/// # 参数
///
/// - page_cache: 与文件inode关联的页缓存
/// - start: 偏移量
pub fn truncate_inode_pages(page_cache: Arc<PageCache>, start: usize) {
    let guard = page_cache.lock_irqsave();
    let pages_count = guard.pages_count();

    for i in start..pages_count {
        let page = guard.get_page(i);
        let page = if let Some(page) = page {
            page
        } else {
            log::warn!("try to truncate page from different page cache");
            return;
        };
        truncate_complete_page(page_cache.clone(), page.clone());
    }
}

fn truncate_complete_page(_page_cache: Arc<PageCache>, page: Arc<Page>) {
    let mut guard = page.write_irqsave();
    guard.remove_flags(PageFlags::PG_DIRTY);
    drop(guard);  // 释放页面锁
    
    // 从页面回收器中移除该页面，避免后续访问已释放的inode
    let paddr = page.phys_address();
    page_reclaimer_lock_irqsave().remove_page(&paddr);
}
