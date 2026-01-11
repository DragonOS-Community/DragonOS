//! mlock 系列系统调用的核心实现
//!
//! # 概述
//!
//! 本模块实现了 mlock 系列系统调用的核心功能，包括：
//! - 单页面的锁定/解锁 (mlock_page, munlock_page)
//! - VMA 范围内的页面遍历和锁定/解锁
//! - 权限检查 (can_do_mlock)
//!
//! # Linux 参考实现
//!
//! 基于 Linux 6.6.21 mm/mlock.c
//!
//! # 关键设计
//!
//! ## 引用计数
//! - 每个物理页面维护 mlock_count 引用计数
//! - 支持多个 VMA 锁定同一页面
//! - 计数从 0→1 时设置 PG_MLOCKED 和 PG_UNEVICTABLE 标志
//! - 计数从 1→0 时清除 PG_MLOCKED 标志
//!
//! ## 页面标志
//! - PG_MLOCKED: 页面已被锁定
//! - PG_UNEVICTABLE: 页面不可被换出（即使不在 LRU 中也会被检查）
//!
//! # TODO
//!
//! - LRU 链表管理：将锁定的页面移到不可换出 LRU

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    mm::{
        page::{page_manager_lock_irqsave, Page, PageFlags},
        ucontext::LockedVMA,
        MemoryManagementArch, PhysAddr, VirtAddr,
    },
    process::{cred::CAPFlags, resource::RLimitID, ProcessManager},
};

/// 检查进程是否有权限执行 mlock 操作
///
/// # Linux 语义
///
/// - 如果进程具有 CAP_IPC_LOCK 权限，允许执行 mlock
/// - 否则，检查 RLIMIT_MEMLOCK 是否为 0
///   - 为 0 表示完全禁止 mlock
///   - 非 0 表示可以在限制范围内执行 mlock
pub fn can_do_mlock() -> bool {
    // 首先检查 CAP_IPC_LOCK 权限
    let cred = ProcessManager::current_pcb().cred();
    if cred.has_capability(CAPFlags::CAP_IPC_LOCK) {
        return true;
    }

    // 没有 CAP_IPC_LOCK 权限，则受 RLIMIT_MEMLOCK 限制
    let rlim = ProcessManager::current_pcb()
        .get_rlimit(RLimitID::Memlock)
        .rlim_cur;

    rlim != 0
}

/// 锁定单个物理页面
///
/// 对指定物理页面增加 mlock 引用计数，并在首次锁定时设置相应标志。
///
/// # 行为
///
/// - 增加 mlock_count 引用计数
/// - 当计数从 0 → 1 时：
///   - 设置 PG_MLOCKED 标志（标记页面已锁定）
///   - 设置 PG_UNEVICTABLE 标志（防止页面被换出）
///
/// # Linux 参考实现
///
/// 基于 Linux 6.6.21 mm/mlock.c:mlock_folio()
/// 该函数在 Linux 中为 void 函数，不会失败
pub fn mlock_page(page: &Arc<Page>) {
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
}

/// 解锁单个物理页面
///
/// 对指定物理页面减少 mlock 引用计数，并在计数归零时清除相应标志。
///
/// # 行为
///
/// - 减少 mlock_count 引用计数
/// - 当计数从 1 → 0 时：
///   - 清除 PG_MLOCKED 标志
///   - 如果页面未被映射（map_count == 0），清除 PG_UNEVICTABLE 标志
///
/// # Linux 参考实现
///
/// 基于 Linux 6.6.21 mm/mlock.c:munlock_folio()
/// 该函数在 Linux 中为 void 函数，不会失败
pub fn munlock_page(page: &Arc<Page>) {
    let mut page_guard = page.write_irqsave();

    // 减少 mlock 计数
    let old_count = page_guard.mlock_count();
    if old_count == 0 {
        return; // 已经解锁，直接返回
    }

    page_guard.dec_mlock_count();

    // 如果计数归零，清除页面标志
    if old_count == 1 {
        // 清除 PG_mlocked
        page_guard.remove_flags(PageFlags::PG_MLOCKED);

        // 如果页面可换出，移回正常 LRU
        // 注意：需要检查页面是否真的可以换出（map_count == 0）
        if page_guard.map_count() == 0 {
            page_guard.remove_flags(PageFlags::PG_UNEVICTABLE);
            // TODO: 从不可换出 LRU 移回可换出 LRU
        }
    }
}

impl LockedVMA {
    /// 对 VMA 范围内的已映射页面应用锁定或解锁操作
    ///
    /// 遍历指定地址范围内的所有已映射页面，对每个页面调用相应的锁定/解锁函数。
    /// 未映射的页面将被跳过（与 Linux 语义一致）。
    ///
    /// # 参数
    ///
    /// - `mapper`: 页表映射器
    /// - `start_addr`: 起始虚拟地址
    /// - `end_addr`: 结束虚拟地址
    /// - `lock`: true=锁定, false=解锁
    ///
    /// # 返回
    ///
    /// 返回已处理的页面数
    pub fn mlock_vma_pages_range(
        &self,
        mapper: &PageMapper,
        start_addr: VirtAddr,
        end_addr: VirtAddr,
        lock: bool,
    ) -> Result<usize, SystemError> {
        let page_count = Self::mlock_walk_page_range(mapper, start_addr, end_addr, 3, lock)?;
        Ok(page_count)
    }

    /// 递归遍历页表，对范围内的页面应用锁定/解锁操作
    ///
    /// 支持多级页表和大页处理。对于大页（huge page），会遍历其中的每个 4K 子页。
    ///
    /// # 参数
    ///
    /// - `mapper`: 页表映射器
    /// - `start_addr`: 起始虚拟地址
    /// - `end_addr`: 结束虚拟地址
    /// - `level`: 当前页表层级（0=叶子页表）
    /// - `lock`: true=锁定, false=解锁
    ///
    /// # 返回
    ///
    /// 返回已处理的页面数
    fn mlock_walk_page_range(
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
                // 大页处理：遍历大页内的每个 4K 子页
                if level > 0 && entry.flags().has_flag(MMArch::ENTRY_FLAG_HUGE_PAGE) {
                    let sub_page_count = (next - start) >> MMArch::PAGE_SHIFT;

                    // 获取大页的基地址
                    let base_paddr = match entry.address() {
                        Ok(paddr) => paddr,
                        Err(_) => continue,
                    };

                    // 计算 start 在当前条目内的偏移
                    let offset_in_entry = start.data() & (entry_size - 1);

                    // 遍历大页中的每个子页
                    for i in 0..sub_page_count {
                        let sub_page_paddr = PhysAddr::new(
                            base_paddr.data() + offset_in_entry + i * MMArch::PAGE_SIZE,
                        );
                        if Self::mlock_phys_page(sub_page_paddr, lock)? {
                            page_count += 1;
                        }
                    }
                } else if level > 0 {
                    // 递归处理下一级页表
                    let sub_pages =
                        Self::mlock_walk_page_range(mapper, start, next, level - 1, lock)?;
                    page_count += sub_pages;
                } else {
                    // 叶子节点（4K 页）
                    match entry.address() {
                        Ok(paddr) => {
                            if Self::mlock_phys_page(paddr, lock)? {
                                page_count += 1;
                            }
                        }
                        Err(_) => {
                            // 页表项不存在，跳过（Linux 语义）
                        }
                    }
                }
            }

            start = next;
        }
        Ok(page_count)
    }

    /// 对物理页面应用锁定/解锁操作
    ///
    /// # 参数
    ///
    /// - `paddr`: 物理地址
    /// - `lock`: true=锁定, false=解锁
    ///
    /// # 返回
    ///
    /// - `Ok(true)`: 成功处理了页面
    /// - `Ok(false)`: 页面不存在于页面管理器中
    fn mlock_phys_page(paddr: PhysAddr, lock: bool) -> Result<bool, SystemError> {
        let mut page_manager_guard = page_manager_lock_irqsave();
        if let Some(page) = page_manager_guard.get(&paddr) {
            drop(page_manager_guard);

            // 对页面应用锁定/解锁（不会失败，与 Linux 一致）
            if lock {
                mlock_page(&page);
            } else {
                munlock_page(&page);
            }

            return Ok(true);
        }
        Ok(false)
    }
}
