use crate::{
    arch::MMArch,
    filesystem::{page_cache::PageCache, vfs::IndexNode},
    libs::{log2::round_up_pow_of_two, ranges::merge_ranges},
    mm::{page::PageFlags, MemoryManagementArch},
};
use alloc::{sync::Arc, vec::Vec};
use num_traits::abs_sub;
use system_error::SystemError;

// 以后其他方面提高了io速度，可以减小到32
pub const MAX_READAHEAD: usize = 128;

/// 文件预读状态
#[derive(Debug, Clone)]
pub struct FileReadaheadState {
    /// 当前预读窗口的起始页索引
    pub start: usize,
    /// 当前预读窗口的总大小（页数）
    pub size: usize,
    /// 异步预读部分的大小（页数）
    pub async_size: usize,
    /// 最大预读窗口大小（页数），可配置
    pub ra_pages: usize,
    /// 上一次访问的页索引（用于顺序判断）
    pub prev_index: i64,
}

impl FileReadaheadState {
    /// 创建新的预读状态
    pub fn new() -> Self {
        Self {
            start: 0,
            size: 0,
            async_size: 0,
            ra_pages: MAX_READAHEAD,
            prev_index: -1,
        }
    }

    /// 第一次顺序读
    /// 可以理解为之前随机读了一次，然后紧接着顺序读下去
    pub fn first_sequential(&self, page_index: usize) -> bool {
        abs_sub(page_index as i64, self.prev_index) <= 1
    }
}

impl Default for FileReadaheadState {
    fn default() -> Self {
        Self::new()
    }
}

/// 预读控制器 - 用于单次预读请求
/// 是栈上的对象，且只会出现在一个线程里面
pub struct ReadaheadControl<'a> {
    /// 关联的 PageCache
    pub page_cache: &'a Arc<PageCache>,
    /// 关联的 Inode
    pub inode: &'a Arc<dyn IndexNode>,
    /// 预读状态的可变引用
    pub ra_state: &'a mut FileReadaheadState,
    /// 本次预读的触发页索引
    pub index: usize,
}

impl<'a> ReadaheadControl<'a> {
    fn get_init_ra_size(req_size: usize, max_pages: usize) -> usize {
        let newsize = round_up_pow_of_two(req_size);

        if newsize < max_pages / 32 {
            newsize * 4
        } else if newsize < max_pages / 4 {
            newsize * 2
        } else {
            newsize
        }
    }

    fn get_next_ra_size(cur_ra_size: usize, max_pages: usize) -> usize {
        if cur_ra_size < max_pages / 16 {
            return 4 * cur_ra_size;
        }
        core::cmp::min(cur_ra_size * 2, max_pages)
    }

    /// 执行实际的页缓存预读
    /// 由于目前DragonOS不支持异步，
    /// 所以直接把整个窗口同步读完
    fn do_page_cache_readahead(&self, number_to_read: usize) -> Result<usize, SystemError> {
        let page_cache = self.page_cache;
        let start_index = self.ra_state.start;

        let set_flag = {
            // 异步窗口的第一个页若未缓存则设置异步标志
            page_cache
                .lock_irqsave()
                .get_page(self.ra_state.start + self.ra_state.size - self.ra_state.async_size)
                .is_none()
        };

        let missing_pages = {
            let page_cache_gaurd = page_cache.lock_irqsave();
            (0..number_to_read)
                .map(|i| start_index + i)
                .filter(|&idx| page_cache_gaurd.get_page(idx).is_none())
                .collect::<Vec<_>>()
        };

        if missing_pages.is_empty() {
            return Ok(0);
        }

        let ranges = merge_ranges(&missing_pages);
        let mut total_read = 0;

        for (page_index, count) in ranges {
            let mut page_buf = alloc::vec![0u8; MMArch::PAGE_SIZE * count];
            let offset = page_index << MMArch::PAGE_SHIFT;

            let read_len = self.inode.read_sync(offset, &mut page_buf)?;

            if read_len == 0 {
                continue;
            }

            page_buf.truncate(read_len);

            let actual_page_count = (read_len + MMArch::PAGE_SIZE - 1) >> MMArch::PAGE_SHIFT;

            let mut page_cache_guard = page_cache.lock_irqsave();
            page_cache_guard.create_pages(page_index, &page_buf)?;
            drop(page_cache_guard);

            total_read += actual_page_count;
        }

        if set_flag {
            if let Some(page) = page_cache
                .lock_irqsave()
                .get_page(self.ra_state.start + self.ra_state.size - self.ra_state.async_size)
            {
                log::debug!("set ra flag at {}", self.ra_state.start + self.ra_state.size - self.ra_state.async_size);
                page.write_irqsave().add_flags(PageFlags::PG_READAHEAD);
            }
        }

        Ok(total_read)
    }

    /// 按需预读算法
    ///
    /// ## 参数
    /// - `ractl`: 预读控制器
    /// - `req_size`: 本次请求的大小（页数）
    /// - `is_async`: 是否是异步触发
    ///
    /// ## 返回值
    /// - `Ok(usize)`: 预读的页数
    pub fn ondemand_readahead(
        &mut self,
        req_size: usize,
        is_async: bool,
    ) -> Result<usize, SystemError> {
        let ra_state = &mut *self.ra_state;
        let start_index = self.index;
        let max_pages = core::cmp::max(ra_state.ra_pages, req_size);
        let file_size = self.inode.metadata()?.size as usize;
        let end_index = if file_size > 0 {
            (file_size - 1) >> MMArch::PAGE_SHIFT
        } else {
            return Ok(0);
        };

        if is_async {
            // 第二次及以后的连续读
            let page_cache_gaurd = self.page_cache.lock_irqsave();
            let next_missing_pages = {
                (start_index..start_index + max_pages)
                    .find(|idx| page_cache_gaurd.get_page(idx.clone()).is_none())
            };
            log::debug!("next_missing_pages: {:?}", next_missing_pages);

            if next_missing_pages.is_none() || next_missing_pages.unwrap() - start_index > max_pages
            {
                return Ok(0);
            }

            ra_state.start = next_missing_pages.unwrap();
            ra_state.size = ra_state.start - start_index + req_size;
            ra_state.size = Self::get_next_ra_size(ra_state.size, max_pages);
            ra_state.async_size = ra_state.size;
        } else if start_index == 0 || ra_state.first_sequential(start_index) {
            // 从头读或者第一次顺序读

            let mut number_to_read = Self::get_init_ra_size(req_size, max_pages);
            if start_index + number_to_read > end_index + 1 {
                number_to_read = (end_index + 1).saturating_sub(start_index);
            }

            if number_to_read == 0 {
                return Ok(0);
            }

            ra_state.start = start_index;
            ra_state.size = number_to_read;
            ra_state.async_size = if ra_state.size > req_size {
                ra_state.size - req_size
            } else {
                ra_state.size
            };
        } else {
            return Ok(0);
        };

        // 避免设置了标记之后立即踩中
        if start_index == ra_state.start && ra_state.size == ra_state.async_size {
            let add_pages = Self::get_next_ra_size(ra_state.size, max_pages);
            if ra_state.size + add_pages <= max_pages {
                ra_state.async_size = add_pages;
                ra_state.size += add_pages;
            } else {
                ra_state.size = max_pages;
                ra_state.async_size = max_pages >> 1;
            }
        }
        // log::debug!("is_async: {}, start: {}, size: {}, async_size: {}", is_async, ra_state.start, ra_state.size, ra_state.async_size);

        let nr_to_read = ra_state.size;
        let read_count = self.do_page_cache_readahead(nr_to_read)?;

        Ok(read_count)
    }
}

/// 同步预读入口 - 缺页时调用
pub fn page_cache_sync_readahead(
    page_cache: &Arc<PageCache>,
    inode: &Arc<dyn IndexNode>,
    ra_state: &mut FileReadaheadState,
    index: usize,
    req_size: usize,
) -> Result<usize, SystemError> {
    let mut ractl = ReadaheadControl {
        page_cache,
        inode,
        ra_state,
        index,
    };

    ractl.ondemand_readahead(req_size, false)
}

/// 异步预读入口 - 命中 PG_READAHEAD 标志时调用
pub fn page_cache_async_readahead(
    page_cache: &Arc<PageCache>,
    inode: &Arc<dyn IndexNode>,
    ra_state: &mut FileReadaheadState,
    index: usize,
    req_size: usize,
) -> Result<usize, SystemError> {
    let mut ractl = ReadaheadControl {
        page_cache,
        inode,
        ra_state,
        index: index + 1, // linux没有+1，但是这里去掉行为就不对了
    };

    ractl.ondemand_readahead(req_size, true)
}
