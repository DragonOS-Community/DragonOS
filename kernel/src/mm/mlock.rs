//! mlock 系列系统调用的核心实现
//!
//! 参考 Linux 6.6.21 mm/mlock.c

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    mm::{
        page::{page_manager_lock_irqsave, Page, PageFlags},
        ucontext::LockedVMA,
        MemoryManagementArch, PhysAddr, VirtAddr, VmFlags,
    },
    process::{resource::RLimitID, ProcessManager},
};

/// 检查进程是否有权限执行 mlock
pub fn can_do_mlock() -> bool {
    let rlim = ProcessManager::current_pcb()
        .get_rlimit(RLimitID::Memlock)
        .rlim_cur;

    if rlim != 0 {
        return true;
    }

    // TODO: 检查 CAP_IPC_LOCK 权限
    // 目前暂时返回 false，后续需要实现权限检查
    false
}

/// 锁定单个页面
///
/// # 参数
/// - `page`: 要锁定的页面
///
/// # 返回
/// - `Ok(())`: 成功
/// - `Err(SystemError)`: 失败
pub fn mlock_page(page: &Arc<Page>) -> Result<(), SystemError> {
    let mut page_guard = page.write_irqsave();

    // 增加 mlock 计数
    let old_count = page_guard.mlock_count();
    page_guard.inc_mlock_count();

    // 如果是第一次锁定，设置页面标志
    if old_count == 0 {
        // 设置 PG_mlocked
        page_guard.add_flags(PageFlags::PG_MLOCKED);

        // 设置 PG_unevictable（防止被换出）
        page_guard.add_flags(PageFlags::PG_UNEVICTABLE);

        // 如果页面在 LRU 中，需要从可换出 LRU 移到不可换出 LRU
        // TODO: 实现 LRU 链表管理
        // 注意：由于页面已设置 PG_UNEVICTABLE 标志，即使没有 LRU 管理，
        // 页面回收机制也会检查该标志，不会被回收
    }

    Ok(())
}

/// 解锁单个页面
///
/// # 参数
/// - `page`: 要解锁的页面
///
/// # 返回
/// - `Ok(())`: 成功
/// - `Err(SystemError)`: 失败
pub fn munlock_page(page: &Arc<Page>) -> Result<(), SystemError> {
    let mut page_guard = page.write_irqsave();

    // 减少 mlock 计数
    let old_count = page_guard.mlock_count();
    if old_count == 0 {
        return Ok(()); // 已经解锁，直接返回
    }

    page_guard.dec_mlock_count();

    // 如果计数归零，清除页面标志
    if old_count == 1 {
        // 清除 PG_mlocked
        page_guard.remove_flags(PageFlags::PG_MLOCKED);

        // 如果页面可换出，移回正常 LRU
        // 注意：需要检查页面是否真的可以换出（map_count == 0）
        if page_guard.can_deallocate() {
            page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
            // TODO: 从不可换出 LRU 移回可换出 LRU
        }
    }

    Ok(())
}

impl LockedVMA {
    /// 对 VMA 范围内的页面应用锁定/解锁
    ///
    /// # 参数
    /// - `mapper`: 页表映射器
    /// - `start_addr`: 起始虚拟地址
    /// - `end_addr`: 结束虚拟地址
    /// - `lock`: true 表示锁定，false 表示解锁
    ///
    /// # 返回
    /// - `Ok(locked_pages)`: 成功，返回已处理的页面数
    /// - `Err(SystemError)`: 失败
    ///
    /// # 说明
    /// 参考 mincore.rs 的遍历逻辑，支持多级页表和大页处理。
    /// 遍历指定地址范围内的所有已映射页面，对每个页面调用 mlock_page 或 munlock_page。
    /// 对于未映射的页面，跳过处理（按 Linux 语义）。
    pub fn mlock_vma_pages_range(
        &self,
        mapper: &PageMapper,
        start_addr: VirtAddr,
        end_addr: VirtAddr,
        lock: bool,
    ) -> Result<usize, SystemError> {
        let page_count = self.mlock_walk_page_range(mapper, start_addr, end_addr, 3, lock)?;
        Ok(page_count)
    }

    /// 递归遍历页表，对范围内的页面应用锁定/解锁
    ///
    /// # 参数
    /// - `mapper`: 页表映射器
    /// - `start_addr`: 起始虚拟地址
    /// - `end_addr`: 结束虚拟地址
    /// - `level`: 当前页表层级
    /// - `lock`: true 表示锁定，false 表示解锁
    ///
    /// # 返回
    /// - 返回已处理的页面数
    fn mlock_walk_page_range(
        &self,
        mapper: &PageMapper,
        start_addr: VirtAddr,
        end_addr: VirtAddr,
        level: usize,
        lock: bool,
    ) -> Result<usize, SystemError> {
        let mut page_count = 0;
        let mut start = start_addr;
        while start < end_addr {
            let entry_size = MMArch::PAGE_SIZE << (level * MMArch::PAGE_ENTRY_SHIFT);
            let next = core::cmp::min(end_addr, start + entry_size);
            if let Some(entry) = mapper.get_entry(start, level) {
                // 大页处理：当上层条目标记为大页时，遍历大页内的每个4K子页
                if level > 0 && entry.flags().has_flag(MMArch::ENTRY_FLAG_HUGE_PAGE) {
                    // 对于大页，需要遍历其中的每个4K子页
                    let sub_page_count = (next - start) >> MMArch::PAGE_SHIFT;
                    // 大页的物理地址是连续的，可以通过偏移计算每个子页的物理地址
                    let base_paddr = match entry.address() {
                        Ok(paddr) => paddr,
                        Err(_) => continue,
                    };
                    for i in 0..sub_page_count {
                        let sub_page_paddr =
                            PhysAddr::new(base_paddr.data() + i * MMArch::PAGE_SIZE);
                        if Self::mlock_phys_page(sub_page_paddr, lock)? {
                            page_count += 1;
                        }
                    }
                } else if level > 0 {
                    // 递归处理下一级页表
                    let sub_pages =
                        self.mlock_walk_page_range(mapper, start, next, level - 1, lock)?;
                    page_count += sub_pages;
                } else {
                    // 叶子节点（4K页）
                    match entry.address() {
                        Ok(paddr) => {
                            if Self::mlock_phys_page(paddr, lock)? {
                                page_count += 1;
                            }
                        }
                        Err(_) => {
                            // 页表项不存在，跳过
                        }
                    }
                }
            }
            // 如果页表项不存在，跳过（按 Linux 语义，不对未映射的页面做任何处理）

            start = next;
        }
        Ok(page_count)
    }

    /// 对物理页面应用锁定/解锁
    ///
    /// # 参数
    /// - `paddr`: 物理地址
    /// - `lock`: true 表示锁定，false 表示解锁
    ///
    /// # 返回
    /// - `Ok(true)`: 成功处理了页面
    /// - `Ok(false)`: 页面不存在或无法处理
    /// - `Err(SystemError)`: 失败
    fn mlock_phys_page(paddr: PhysAddr, lock: bool) -> Result<bool, SystemError> {
        let mut page_manager_guard = page_manager_lock_irqsave();
        if let Some(page) = page_manager_guard.get(&paddr) {
            drop(page_manager_guard);

            // 对页面应用锁定/解锁
            if lock {
                mlock_page(&page)?;
            } else {
                munlock_page(&page)?;
            }

            return Ok(true);
        }
        Ok(false)
    }
}
