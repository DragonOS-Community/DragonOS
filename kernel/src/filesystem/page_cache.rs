use core::{
    cmp::min,
    sync::atomic::{AtomicUsize, Ordering},
};

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use hashbrown::HashMap;
use system_error::SystemError;

use super::vfs::IndexNode;
use crate::libs::spinlock::SpinLockGuard;
use crate::mm::page::FileMapInfo;
use crate::{arch::mm::LockedFrameAllocator, libs::lazy_init::Lazy};
use crate::{
    arch::MMArch,
    libs::spinlock::SpinLock,
    mm::{
        page::{page_manager_lock_irqsave, page_reclaimer_lock_irqsave, Page, PageFlags},
        MemoryManagementArch,
    },
};
use crate::{libs::align::page_align_up, mm::page::PageType};

static PAGE_CACHE_ID: AtomicUsize = AtomicUsize::new(0);
/// 页面缓存
#[derive(Debug)]
pub struct PageCache {
    id: usize,
    inner: SpinLock<InnerPageCache>,
    inode: Lazy<Weak<dyn IndexNode>>,
}

#[derive(Debug)]
pub struct InnerPageCache {
    #[allow(unused)]
    id: usize,
    pages: HashMap<usize, Arc<Page>>,
    page_cache_ref: Weak<PageCache>,
}

impl InnerPageCache {
    pub fn new(page_cache_ref: Weak<PageCache>, id: usize) -> InnerPageCache {
        Self {
            id,
            pages: HashMap::new(),
            page_cache_ref,
        }
    }

    pub fn add_page(&mut self, offset: usize, page: &Arc<Page>) {
        self.pages.insert(offset, page.clone());
    }

    pub fn get_page(&self, offset: usize) -> Option<Arc<Page>> {
        self.pages.get(&offset).cloned()
    }

    pub fn remove_page(&mut self, offset: usize) -> Option<Arc<Page>> {
        self.pages.remove(&offset)
    }

    fn create_pages(&mut self, start_page_index: usize, buf: &[u8]) -> Result<(), SystemError> {
        assert!(buf.len() % MMArch::PAGE_SIZE == 0);

        let page_num = buf.len() / MMArch::PAGE_SIZE;

        let len = buf.len();
        if len == 0 {
            return Ok(());
        }

        let mut page_manager_guard = page_manager_lock_irqsave();

        for i in 0..page_num {
            let buf_offset = i * MMArch::PAGE_SIZE;
            let page_index = start_page_index + i;

            let page = page_manager_guard.create_one_page(
                PageType::File(FileMapInfo {
                    page_cache: self
                        .page_cache_ref
                        .upgrade()
                        .expect("failed to get self_arc of pagecache"),
                    index: page_index,
                }),
                PageFlags::PG_LRU,
                &mut LockedFrameAllocator,
            )?;

            let mut page_guard = page.write_irqsave();
            unsafe {
                page_guard.copy_from_slice(&buf[buf_offset..buf_offset + MMArch::PAGE_SIZE]);
            }

            self.add_page(page_index, &page);
        }

        Ok(())
    }

    /// 从PageCache中读取数据。
    ///
    /// ## 参数
    ///
    /// - `offset` 偏移量
    /// - `buf` 缓冲区
    ///
    /// ## 返回值
    ///
    /// - `Ok(usize)` 成功读取的长度
    /// - `Err(SystemError)` 失败返回错误码
    pub fn read(&mut self, offset: usize, buf: &mut [u8]) -> Result<usize, SystemError> {
        let inode = self
            .page_cache_ref
            .upgrade()
            .unwrap()
            .inode
            .upgrade()
            .unwrap();
        let file_size = inode.metadata().unwrap().size;

        let len = if offset < file_size as usize {
            core::cmp::min(file_size as usize, offset + buf.len()) - offset
        } else {
            0
        };

        if len == 0 {
            return Ok(0);
        }

        let mut not_exist = Vec::new();

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let page_num = (page_align_up(offset + len) >> MMArch::PAGE_SHIFT) - start_page_index;

        let mut buf_offset = 0;
        let mut ret = 0;
        for i in 0..page_num {
            let page_index = start_page_index + i;

            // 第一个页可能需要计算页内偏移
            let page_offset = if i == 0 {
                offset % MMArch::PAGE_SIZE
            } else {
                0
            };

            // 第一个页和最后一个页可能不满
            let sub_len = if i == 0 {
                min(len, MMArch::PAGE_SIZE - page_offset)
            } else if i == page_num - 1 {
                (offset + len - 1) % MMArch::PAGE_SIZE + 1
            } else {
                MMArch::PAGE_SIZE
            };

            if let Some(page) = self.get_page(page_index) {
                let sub_buf = &mut buf[buf_offset..(buf_offset + sub_len)];
                unsafe {
                    sub_buf.copy_from_slice(
                        &page.read_irqsave().as_slice()[page_offset..page_offset + sub_len],
                    );
                }
                ret += sub_len;
            } else if let Some((index, count)) = not_exist.last_mut() {
                if *index + *count == page_index {
                    *count += 1;
                } else {
                    not_exist.push((page_index, 1));
                }
            } else {
                not_exist.push((page_index, 1));
            }

            buf_offset += sub_len;
        }

        for (page_index, count) in not_exist {
            // TODO 这里使用buffer避免多次读取磁盘，将来引入异步IO直接写入页面，减少内存开销和拷贝
            let mut page_buf = vec![0u8; MMArch::PAGE_SIZE * count];
            inode.read_sync(page_index * MMArch::PAGE_SIZE, page_buf.as_mut())?;

            self.create_pages(page_index, page_buf.as_mut())?;

            // 实际要拷贝的内容在文件中的偏移量
            let copy_offset = core::cmp::max(page_index * MMArch::PAGE_SIZE, offset);
            // 实际要拷贝的内容的长度
            let copy_len = core::cmp::min((page_index + count) * MMArch::PAGE_SIZE, offset + len)
                - copy_offset;

            let page_buf_offset = if page_index * MMArch::PAGE_SIZE < copy_offset {
                copy_offset - page_index * MMArch::PAGE_SIZE
            } else {
                0
            };

            let buf_offset = copy_offset.saturating_sub(offset);

            buf[buf_offset..buf_offset + copy_len]
                .copy_from_slice(&page_buf[page_buf_offset..page_buf_offset + copy_len]);

            ret += copy_len;

            // log::debug!("page_offset:{page_offset}, count:{count}");
            // log::debug!("copy_offset:{copy_offset}, copy_len:{copy_len}");
            // log::debug!("buf_offset:{buf_offset}, page_buf_offset:{page_buf_offset}");
        }

        Ok(ret)
    }

    /// 向PageCache中写入数据。
    ///
    /// ## 参数
    ///
    /// - `offset` 偏移量
    /// - `buf` 缓冲区
    ///
    /// ## 返回值
    ///
    /// - `Ok(usize)` 成功读取的长度
    /// - `Err(SystemError)` 失败返回错误码
    pub fn write(&mut self, offset: usize, buf: &[u8]) -> Result<usize, SystemError> {
        let len = buf.len();
        if len == 0 {
            return Ok(0);
        }

        // log::debug!("offset:{offset}, len:{len}");

        let start_page_index = offset >> MMArch::PAGE_SHIFT;
        let page_num = (page_align_up(offset + len) >> MMArch::PAGE_SHIFT) - start_page_index;

        let mut buf_offset = 0;
        let mut ret = 0;

        for i in 0..page_num {
            let page_index = start_page_index + i;

            // 第一个页可能需要计算页内偏移
            let page_offset = if i == 0 {
                offset % MMArch::PAGE_SIZE
            } else {
                0
            };

            // 第一个页和最后一个页可能不满
            let sub_len = if i == 0 {
                min(len, MMArch::PAGE_SIZE - page_offset)
            } else if i == page_num - 1 {
                (offset + len - 1) % MMArch::PAGE_SIZE + 1
            } else {
                MMArch::PAGE_SIZE
            };

            let mut page = self.get_page(page_index);

            if page.is_none() {
                let page_buf = vec![0u8; MMArch::PAGE_SIZE];
                self.create_pages(page_index, &page_buf)?;
                page = self.get_page(page_index);
            }

            if let Some(page) = page {
                let sub_buf = &buf[buf_offset..(buf_offset + sub_len)];
                let mut page_guard = page.write_irqsave();
                unsafe {
                    page_guard.as_slice_mut()[page_offset..page_offset + sub_len]
                        .copy_from_slice(sub_buf);
                }
                page_guard.add_flags(PageFlags::PG_DIRTY);

                ret += sub_len;

                // log::debug!(
                //     "page_offset:{page_offset}, buf_offset:{buf_offset}, sub_len:{sub_len}"
                // );
            } else {
                return Err(SystemError::EIO);
            };

            buf_offset += sub_len;
        }
        Ok(ret)
    }

    pub fn resize(&mut self, len: usize) -> Result<(), SystemError> {
        let page_num = page_align_up(len) / MMArch::PAGE_SIZE;

        let mut reclaimer = page_reclaimer_lock_irqsave();
        for (_i, page) in self.pages.drain_filter(|index, _page| *index >= page_num) {
            let _ = reclaimer.remove_page(&page.phys_address());
        }

        if page_num > 0 {
            let last_page_index = page_num - 1;
            let last_len = len - last_page_index * MMArch::PAGE_SIZE;
            if let Some(page) = self.get_page(last_page_index) {
                unsafe {
                    page.write_irqsave().truncate(last_len);
                };
            } else {
                return Err(SystemError::EIO);
            }
        }

        Ok(())
    }
}

impl Drop for InnerPageCache {
    fn drop(&mut self) {
        log::debug!("page cache drop");
        let mut page_manager = page_manager_lock_irqsave();
        for page in self.pages.values() {
            page_manager.remove_page(&page.phys_address());
        }
    }
}

impl PageCache {
    pub fn new(inode: Option<Weak<dyn IndexNode>>) -> Arc<PageCache> {
        let id = PAGE_CACHE_ID.fetch_add(1, Ordering::SeqCst);
        Arc::new_cyclic(|weak| Self {
            id,
            inner: SpinLock::new(InnerPageCache::new(weak.clone(), id)),
            inode: {
                let v: Lazy<Weak<dyn IndexNode>> = Lazy::new();
                if let Some(inode) = inode {
                    v.init(inode);
                }
                v
            },
        })
    }

    /// # 获取页缓存的ID
    #[inline]
    #[allow(unused)]
    pub fn id(&self) -> usize {
        self.id
    }

    pub fn inode(&self) -> Option<Weak<dyn IndexNode>> {
        self.inode.try_get().cloned()
    }

    pub fn set_inode(&self, inode: Weak<dyn IndexNode>) -> Result<(), SystemError> {
        if self.inode.initialized() {
            return Err(SystemError::EINVAL);
        }
        self.inode.init(inode);
        Ok(())
    }

    pub fn lock_irqsave(&self) -> SpinLockGuard<InnerPageCache> {
        self.inner.lock_irqsave()
    }
}
