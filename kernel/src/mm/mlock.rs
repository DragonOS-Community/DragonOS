//! mlock 系列系统调用的核心实现
//!
//! 参考 Linux 6.6.21 mm/mlock.c

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    mm::{
        page::{Page, PageFlags},
        ucontext::{LockedVMA},
        VmFlags,
        VirtAddr
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

/// 对 VMA 范围内的页面应用锁定
///
/// # 参数
/// - `vma`: 要处理的 VMA
/// - `start`: 起始虚拟地址
/// - `end`: 结束虚拟地址
/// - `lock`: true 表示锁定，false 表示解锁
///
/// # 返回
/// - `Ok(())`: 成功
/// - `Err(SystemError)`: 失败
pub fn mlock_vma_pages_range(
    _vma: &Arc<LockedVMA>,
    _start: VirtAddr,
    _end: VirtAddr,
    _lock: bool,
) -> Result<(), SystemError> {
    // TODO: 实现页表遍历和页面锁定/解锁
    // 需要遍历 VMA 范围内的所有页面，对每个页面调用 mlock_page 或 munlock_page

    Ok(())
}

/// 应用 VMA 锁定标志（分割/合并 VMA）
///
/// # 参数
/// - `_addr_space`: 地址空间
/// - `_start`: 起始虚拟地址
/// - `_end`: 结束虚拟地址
/// - `_new_flags`: 新的 VMA 标志
///
/// # 返回
/// - `Ok(())`: 成功
/// - `Err(SystemError)`: 失败
pub fn apply_vma_lock_flags(
    _addr_space: &Arc<crate::mm::ucontext::AddressSpace>,
    _start: VirtAddr,
    _end: VirtAddr,
    _new_flags: VmFlags,
) -> Result<(), SystemError> {
    // TODO: 实现 VMA 分割和合并逻辑
    // 参考 Linux 的 mlock_fixup() 函数

    Ok(())
}

/// 应用 mlockall 标志
///
/// # 参数
/// - `_addr_space`: 地址空间
/// - `_flags`: MCL_* 标志
///
/// # 返回
/// - `Ok(())`: 成功
/// - `Err(SystemError)`: 失败
pub fn apply_mlockall_flags(
    _addr_space: &Arc<crate::mm::ucontext::AddressSpace>,
    _flags: u32,
) -> Result<(), SystemError> {
    // TODO: 实现 mlockall 标志应用逻辑

    Ok(())
}
