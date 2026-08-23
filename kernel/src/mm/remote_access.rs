//! 跨地址空间的用户内存远程访问统一入口。
//!
//! ptrace 的 PEEKDATA/POKEDATA、`/proc/[pid]/mem` 与 `process_vm_readv/writev`
//! 三条路径统一经 [`AddressSpace::access_remote_vm`] 访问目标进程内存，
//! 对应 Linux 的 `__access_remote_vm`：
//! - 读锁内逐页做 VMA 权限检查与页表翻译，present 页直接拷贝（跨页不重取锁）；
//! - 未映射或只读页释放读锁后走缺页处理（`FAULT_FLAG_REMOTE`）再重试，
//!   每个地址最多 fault-in 一次，防死循环；
//! - 不可访问处按短拷贝终止；首字节即不可访问返回 `EIO`，由调用方映射为
//!   各自的 errno（ptrace/proc-mem 保持 EIO，process_vm 映射为 EFAULT）。

use crate::{
    arch::MMArch,
    filesystem::page_cache::PageCache,
    mm::{
        fault,
        page::{Page, PageFlags, PageType},
        ucontext::{AddressSpace, LockedVMA},
        MemoryManagementArch, PhysAddr, VirtAddr, VmFaultReason, VmFlags,
    },
};
use alloc::sync::Arc;
use system_error::SystemError;

/// 远程访问的方向与数据缓冲。
#[derive(Debug)]
pub enum RemoteAccess<'a> {
    /// 读：把目标地址空间的内容拷入该缓冲。
    Read(&'a mut [u8]),
    /// 写：把该缓冲的内容拷入目标地址空间。
    Write(&'a [u8]),
}

impl RemoteAccess<'_> {
    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::Read(buf) => buf.len(),
            Self::Write(buf) => buf.len(),
        }
    }

    #[inline]
    fn is_write(&self) -> bool {
        matches!(self, Self::Write(_))
    }
}

impl AddressSpace {
    /// 跨地址空间访问用户内存。
    ///
    /// - `force=true`：ptrace 与 `/proc/[pid]/mem` 语义——允许写只读的私有映射（落 COW 缺页），允许读无 `VM_READ` 的映射
    /// - `force=false`：`process_vm_*` 语义——写无 `VM_WRITE` 的映射直接失败。
    ///
    /// 返回实际访问的字节数；目标区间中途不可访问按短拷贝返回 `Ok(copied)`，
    /// 首字节即不可访问返回 `Err(EIO)`。
    pub fn access_remote_vm(
        self: &Arc<Self>,
        addr: usize,
        mut dir: RemoteAccess<'_>,
        force: bool,
    ) -> Result<usize, SystemError> {
        let total = dir.len();
        let write = dir.is_write();
        if total == 0 {
            return Ok(0);
        }
        // 尚未接入统一指令缓存同步设施的架构上一律拒绝远程写：
        #[cfg(not(target_arch = "x86_64"))]
        if write {
            return Err(SystemError::EIO);
        }

        let mut copied = 0usize;
        // 每个地址最多 fault-in 一次：记录上一次 fault-in 的地址，避免死循环。
        let mut faulted_at: Option<usize> = None;

        while copied < total {
            let cur = match addr.checked_add(copied) {
                Some(c) => c,
                None => return Ok(copied),
            };
            if cur >= MMArch::USER_END_VADDR.data() {
                return Ok(copied);
            }

            // 读锁内：连续处理尽可能多的已映射页（跨页不重取锁）。
            // hard_stop 表示本页命中无法经缺页恢复的终止条件（无 VMA / VMA 权限不满足 / 脏发布失败），直接按短拷贝结束。
            let (advanced, hard_stop) = {
                let guard = self.read();
                let mut local = 0usize;
                let mut hard_stop = false;
                while copied + local < total {
                    let p = match addr.checked_add(copied + local) {
                        Some(v) => v,
                        None => break,
                    };
                    if p >= MMArch::USER_END_VADDR.data() {
                        break;
                    }
                    let page_off = p & (MMArch::PAGE_SIZE - 1);
                    // 本页内可拷字节：每页一次钳位。
                    let chunk =
                        core::cmp::min(total - (copied + local), MMArch::PAGE_SIZE - page_off);
                    let pvaddr = VirtAddr::new(p);
                    // 每页一次 VMA 校验 + 一次页表翻译。
                    let Some(vma) = guard.mappings.contains(pvaddr) else {
                        break;
                    };
                    if !vma_permits(&vma, write, force) {
                        hard_stop = true;
                        break;
                    }
                    let Some((phys_frame, entry_flags)) =
                        guard.user_mapper.utable.translate(pvaddr)
                    else {
                        break;
                    };
                    if write && !entry_flags.has_write() && !(force && faulted_at == Some(p)) {
                        // 只读页：写需先落 COW 缺页
                        break;
                    }
                    if !entry_flags.has_user() && (write || !force) {
                        // PROT_NONE 编码为“present 但无用户访问位”的页表项：
                        // 仅强制读允许跟随；非强制访问与强制写一律终止。
                        break;
                    }
                    // 写文件页：先解析并钉住页缓存页（跨拷贝持有 Arc），数据落笔后再发布脏状态
                    let pinned = if write {
                        match pin_file_page_for_dirty(&vma, pvaddr, &phys_frame) {
                            Ok(p) => p,
                            Err(_) => {
                                hard_stop = true;
                                break;
                            }
                        }
                    } else {
                        None
                    };
                    let Some(kernel_base) = (unsafe {
                        MMArch::phys_2_virt(PhysAddr::new(phys_frame.data() + page_off))
                    }) else {
                        break;
                    };
                    let kvptr = kernel_base.data() as *mut u8;
                    unsafe {
                        match &mut dir {
                            RemoteAccess::Read(rb) => core::ptr::copy_nonoverlapping(
                                kvptr as *const u8,
                                rb[copied + local..copied + local + chunk].as_mut_ptr(),
                                chunk,
                            ),
                            RemoteAccess::Write(wb) => core::ptr::copy_nonoverlapping(
                                wb[copied + local..copied + local + chunk].as_ptr(),
                                kvptr,
                                chunk,
                            ),
                        }
                    }
                    local += chunk;
                    if let Some((page_cache, page, page_index)) = pinned.as_ref() {
                        if publish_pinned_page_dirty(page_cache, page, *page_index).is_err() {
                            // 脏记账失败：数据已落但无脏标记，终止本块，由后续回写前的再校验兜底。
                            hard_stop = true;
                            break;
                        }
                    }
                    // 跨页后不 break：同一读锁内继续处理下一页。
                }
                (local, hard_stop)
            }; // 读锁 drop

            if advanced > 0 {
                copied += advanced;
                faulted_at = None; // 有进展，重置 fault 记录。
            }
            if hard_stop {
                return finish_short(copied);
            }
            if advanced > 0 {
                continue;
            }

            // 当前页未映射/需 COW：读锁已 drop，锁外 fault-in 后重试。
            if faulted_at == Some(cur) {
                // 本地址已 fault-in 过仍无推进：按短拷贝终止。
                return finish_short(copied);
            }
            match fault_in_remote_page(self, VirtAddr::new(cur), write) {
                Ok(()) => {
                    faulted_at = Some(cur);
                    // 重新取读锁重试本页。
                }
                Err(_) => return finish_short(copied),
            }
        }
        Ok(copied)
    }
}

/// 短拷贝终止的统一出口：已有进展返回已拷字节数，首字节即失败返回 EIO。
#[inline]
fn finish_short(copied: usize) -> Result<usize, SystemError> {
    if copied > 0 {
        Ok(copied)
    } else {
        Err(SystemError::EIO)
    }
}

/// 读锁内逐页的 VMA 权限检查。
fn vma_permits(vma: &Arc<LockedVMA>, write: bool, force: bool) -> bool {
    let vm_flags = *vma.lock().vm_flags();
    if write {
        // 强制写只越过“写保护”（只读私有映射的 COW），对共享映射
        // 无效；无 VM_WRITE 的私有映射按 COW 放行。
        vm_flags.contains(VmFlags::VM_WRITE)
            || (force
                && vm_flags.contains(VmFlags::VM_MAYWRITE)
                && !vm_flags.contains(VmFlags::VM_SHARED))
    } else {
        vm_flags.contains(VmFlags::VM_READ) || (force && vm_flags.contains(VmFlags::VM_MAYREAD))
    }
}
/// 脏发布所需的页缓存定位：缓存、受管页与其在缓存中的索引。
type PinnedDirtyPage = (Arc<PageCache>, Arc<Page>, usize);

/// 写文件映射的 present 可写页时，先解析并钉住待弄脏的页缓存页。
fn pin_file_page_for_dirty(
    vma: &Arc<LockedVMA>,
    addr: VirtAddr,
    mapped_frame: &PhysAddr,
) -> Result<Option<PinnedDirtyPage>, SystemError> {
    let (page_cache, page_index) = {
        let vma_guard = vma.lock();
        let Some(file) = vma_guard.vm_file() else {
            // 非文件映射（匿名页等）：直拷，无需脏发布。
            return Ok(None);
        };
        let Some(base_pgoff) = vma_guard.backing_page_offset() else {
            return Err(SystemError::EIO);
        };
        let region_start = vma_guard.region().start().data();
        let page_cache = file.inode().page_cache();
        (
            page_cache,
            base_pgoff + ((addr.data() - region_start) >> MMArch::PAGE_SHIFT),
        )
    };

    let Some(page_cache) = page_cache else {
        // 无页缓存（设备映射等）：直拷。
        return Ok(None);
    };
    // 只读快照定位该偏移的受管页，并确认其与页表映射的是同一物理帧。
    let Some(page) = page_cache.manager().get_page_any(page_index) else {
        return Ok(None);
    };
    let page_type = page.read().page_type().clone();
    let PageType::File(info) = page_type else {
        return Ok(None);
    };
    if page.phys_address() != *mapped_frame {
        // 私有 COW 拷贝或索引已被换页：不弄脏页缓存，直拷。
        return Ok(None);
    }
    let Some(page_cache) = info.page_cache.upgrade() else {
        return Ok(None);
    };
    Ok(Some((page_cache, page, page_index)))
}

/// 拷贝落笔之后发布页缓存脏状态（索引由解析阶段给出，不再重复推导）。
fn publish_pinned_page_dirty(
    page_cache: &Arc<PageCache>,
    page: &Arc<Page>,
    page_index: usize,
) -> Result<(), SystemError> {
    let mut dirty_reservation = page_cache.prepare_page_dirty()?;
    // 持页锁贯穿脏发布：回写完成侧在同一锁下采样 PG_DIRTY，
    // 防止本次写入被并入更早一轮回写化身。
    let mut page_locked = page.write();
    page_locked.add_flags(PageFlags::PG_DIRTY);
    if page_cache
        .mark_page_dirty_prepared_page_locked(page_index, &mut dirty_reservation, &page_locked)
        .is_err()
    {
        page_locked.remove_flags(PageFlags::PG_DIRTY);
        return Err(SystemError::EIO);
    }
    Ok(())
}

/// 在目标地址空间中 fault-in 一个页（调用前必须已释放 mm 读锁）。
fn fault_in_remote_page(
    target_vm: &Arc<AddressSpace>,
    address: VirtAddr,
    write: bool,
) -> Result<(), SystemError> {
    let mut flags = fault::FaultFlags::FAULT_FLAG_REMOTE;
    if write {
        flags |= fault::FaultFlags::FAULT_FLAG_WRITE;
    }

    // 对文件映射 VMA 预建页缓存，确保后续缺页处理能命中。
    let file_backed_vma = {
        let space_guard = target_vm.read();
        let Some(vma) = space_guard.mappings.find_nearest(address) else {
            return Err(SystemError::EIO);
        };
        let vma_guard = vma.lock();
        if vma_guard.region().contains(address) && vma_guard.vm_file().is_some() {
            Some(vma.clone())
        } else {
            None
        }
    };

    if let Some(vma) = file_backed_vma {
        prefault_file_backing(&vma, address)?;
    }

    // 统一缺页处理。
    let mut space_guard = target_vm.write();
    let Some(vma) = space_guard.mappings.find_nearest(address) else {
        return Err(SystemError::EIO);
    };

    let vma_guard = vma.lock();
    let region = *vma_guard.region();
    let vm_flags = *vma_guard.vm_flags();
    drop(vma_guard);

    if !region.contains(address) {
        // 地址落在最近 VMA 之外
        if !vm_flags.contains(VmFlags::VM_GROWSDOWN) {
            return Err(SystemError::EIO);
        }
        let extension_size = region.start().data() - address.data();
        let max_stack_limit = space_guard
            .user_stack
            .as_ref()
            .map(|s| s.max_limit())
            .unwrap_or(0);
        if extension_size > max_stack_limit || !space_guard.can_extend_stack(extension_size) {
            return Err(SystemError::EIO);
        }
        let post_commit = space_guard
            .extend_stack(extension_size)
            .map_err(|_| SystemError::EIO)?;
        drop(space_guard);
        if let Some(request) = post_commit {
            let _ = target_vm.populate_locked_vma_post_commit(request);
        }
        return fault_in_remote_page(target_vm, address, write);
    }

    let fault_result = unsafe {
        let mm = space_guard.outer_addr_space().ok_or(SystemError::EFAULT)?;
        let mapper = &mut space_guard.user_mapper.utable;
        fault::PageFaultHandler::handle_mm_fault(fault::PageFaultMessage::new(
            vma, address, flags, mapper, mm,
        ))
    };

    if fault_result
        .reason
        .contains(VmFaultReason::VM_FAULT_COMPLETED)
    {
        Ok(())
    } else {
        Err(SystemError::EIO)
    }
}

/// 预建文件映射 VMA 的页缓存页。
fn prefault_file_backing(vma: &Arc<LockedVMA>, address: VirtAddr) -> Result<(), SystemError> {
    let (file, base_pgoff, region_start) = {
        let vma_guard = vma.lock();
        let file = vma_guard.vm_file().ok_or(SystemError::EIO)?;
        let base_pgoff = vma_guard.backing_page_offset().ok_or(SystemError::EIO)?;
        (file, base_pgoff, vma_guard.region().start().data())
    };

    let page_index = base_pgoff + ((address.data() - region_start) >> MMArch::PAGE_SHIFT);
    let inode = file.inode();
    let file_size = inode.metadata()?.size.max(0) as usize;
    if file_size == 0 || page_index.saturating_mul(MMArch::PAGE_SIZE) >= file_size {
        return Err(SystemError::EIO);
    }

    let page_cache = inode.page_cache().ok_or(SystemError::EIO)?;
    let _ = page_cache.manager().commit_page(page_index)?;
    Ok(())
}
