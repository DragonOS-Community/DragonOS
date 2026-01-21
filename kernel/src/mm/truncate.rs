use super::page::{Page, PageFlags};
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
    let pages_count = page_cache.manager().pages_count().unwrap_or(0);

    for i in start..pages_count {
        let page = page_cache.manager().get_page_any(i);
        let page = if let Some(page) = page {
            page
        } else {
            log::warn!("try to truncate page from different page cache");
            return;
        };
        truncate_complete_page(page_cache.clone(), i, page.clone());
    }
}

fn truncate_complete_page(page_cache: Arc<PageCache>, page_index: usize, page: Arc<Page>) {
    let mut guard = page.write();
    guard.remove_flags(PageFlags::PG_DIRTY);
    drop(guard);
    page_cache.mark_page_uptodate(page_index);
}
