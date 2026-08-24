//! Per-mm uprobe 管理 + XOL 区 + 断点页安装（计划步骤 3+4）。
//!
//! 本模块提供 uprobe 注册/注销的基础设施，供 batch3（异常分发）与 batch4（perf 接入）调用。
//!
//! # 关键设计（评审 findings）
//!
//! - **F8**：`uprobe_list` / `xol_area` / `uprobe_page_state` 挂在 `AddressSpace` 上，由
//!   **独立 irqsave `SpinLock`** 保护（**不**走 `inner: RwSem`），命中路径（#BP/#DB 关中断）
//!   仅 `lock_irqsave` + 查表，绝不睡眠。
//! - **F1/F2**：断点页安装复刻 `do_wp_page` 私有文件 COW——`copy_page_as_normal` + patch
//!   0xcc + **单次** `set_entry` 原子帧替换（**绝不** unmap+map_phys 制造瞬时空 PTE）+
//!   `insert_vma`/`remove_vma` rmap 账簿 + `flush_tlb_range`。
//! - **F7**：每目标 mm 私有 COW 副本（type `Normal`），**绝不**修改共享 page-cache 页
//!   （否则 writeback 回写 0xcc 损坏 .so）。
//! - **F6 装弹顺序不变量**：注册时严格按 XOL slot 分配 → uprobe 表项插入 → 0xcc 页发布；
//!   0xcc 发布前任何路径查该 vaddr 必须能找到就绪 uprobe 表项。
use crate::libs::{spinlock::SpinLock, wait_queue::WaitQueue};
use alloc::{
    collections::BTreeMap,
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, AtomicUsize, Ordering};

use system_error::SystemError;

use crate::{
    arch::{mm::PageMapper, MMArch},
    filesystem::{
        page_cache::PageCache,
        vfs::{file::File, IndexNode},
    },
    libs::mutex::Mutex,
    mm::{
        page::{page_manager_lock, Page, PageEntry},
        syscall::{MapFlags, ProtFlags},
        MemoryManagementArch, PhysAddr, VirtAddr, VirtRegion, VmFlags,
    },
    process::{ProcessControlBlock, ProcessFlags},
};

use super::RwSemWriteGuard;
use super::{AddressSpace, InnerAddressSpace, LockedVMA};

use uprobe::{analyze_insn, build_xol_slot, InsnAnalysis, UPROBE_INSN_COPY_SIZE};

mod consumer;
mod definition;
mod reconcile;
mod site;
mod xol;

pub use consumer::*;
pub use definition::*;
pub use site::*;
pub use xol::*;

pub use reconcile::*;

/// Whether an enabled consumer can require executable mapping publication to
/// synchronize with uprobe installation.
pub(super) fn requires_exec_publication_barrier(
    file: &Arc<File>,
    flags: VmFlags,
    file_start_byte: usize,
    len: usize,
) -> bool {
    if !site::valid_probe_vma_flags(flags) || consumer::uprobe_registry_is_empty() {
        return false;
    }
    let Some(page_cache) = file.inode().page_cache() else {
        return false;
    };
    let Some(file_end) = file_start_byte.checked_add(len) else {
        return false;
    };
    let query_start = file_start_byte.saturating_sub(UPROBE_INSN_COPY_SIZE - 1);
    consumer::uprobe_registry_has_active_range(
        Arc::as_ptr(&page_cache) as usize,
        query_start,
        file_end,
    )
}
