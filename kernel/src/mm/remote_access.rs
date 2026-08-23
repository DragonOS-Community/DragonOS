//! Unified entry for remote access to user memory across address spaces.
//!
//! ptrace PEEKDATA/POKEDATA, `/proc/[pid]/mem` and `process_vm_readv/writev`
//! all reach the target's memory via [`AddressSpace::access_remote_vm`]:
//! Behavior:
//! - VMA permission check and page-table walk per page under the read lock; present pages are copied directly (no re-lock across pages);
//! - unmapped or read-only pages are faulted in (`FAULT_FLAG_REMOTE`) after dropping the read lock, then retried;
//!   each address is faulted in at most once to avoid livelock;
//! - inaccessible ranges end as a short copy; an inaccessible first byte returns `EIO`, mapped by
//!   the caller to its own errno (EIO for ptrace/proc-mem, EFAULT for process_vm).

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

/// Direction and buffer of a remote access.
#[derive(Debug)]
pub enum RemoteAccess<'a> {
    /// Read: copy target memory into this buffer.
    Read(&'a mut [u8]),
    /// Write: copy this buffer into target memory.
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
    /// Access user memory across address spaces.
    ///
    /// - `force=true`: ptrace and `/proc/[pid]/mem` semantics — may write read-only private mappings (COW) and read mappings without `VM_READ`
    /// - `force=false`: `process_vm_*` semantics — writing mappings without `VM_WRITE` fails.
    ///
    /// Returns the number of bytes accessed; an inaccessible middle returns a short `Ok(copied)`,
    /// an inaccessible first byte returns `Err(EIO)`.
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
        // Reject remote writes on archs without unified icache sync:
        #[cfg(not(target_arch = "x86_64"))]
        if write {
            return Err(SystemError::EIO);
        }

        let mut copied = 0usize;
        // Fault in each address at most once: remember the last faulted address to avoid livelock.
        let mut faulted_at: Option<usize> = None;

        while copied < total {
            let cur = match addr.checked_add(copied) {
                Some(c) => c,
                None => return Ok(copied),
            };
            if cur >= MMArch::USER_END_VADDR.data() {
                return Ok(copied);
            }

            // Under the read lock: process as many mapped pages as possible.
            // hard_stop marks a condition faulting cannot fix (no VMA / permission denied / dirty-publish failure): end as a short copy.
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
                    // Bytes copyable within this page: clamp once per page.
                    let chunk =
                        core::cmp::min(total - (copied + local), MMArch::PAGE_SIZE - page_off);
                    let pvaddr = VirtAddr::new(p);
                    // One VMA check + one page-table walk per page.
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
                        // Read-only page: write must COW-fault first
                        break;
                    }
                    if !entry_flags.has_user() && (write || !force) {
                        // PROT_NONE is encoded as present-without-user PTEs:
                        // only forced reads may follow; other accesses and forced writes stop.
                        break;
                    }
                    // File page write: pin the page-cache page first (Arc held across the copy), publish dirty afterwards
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
                            // Dirty accounting failed: data written but unmarked; end this block, revalidation before writeback covers it.
                            hard_stop = true;
                            break;
                        }
                    }
                    // No break across pages: keep going under the same read lock.
                }
                (local, hard_stop)
            }; // read lock dropped

            if advanced > 0 {
                copied += advanced;
                faulted_at = None; // progress: reset the fault record.
            }
            if hard_stop {
                return finish_short(copied);
            }
            if advanced > 0 {
                continue;
            }

            // Page unmapped/needs COW: lock dropped, fault in outside the lock and retry.
            if faulted_at == Some(cur) {
                // Already faulted here without progress: end as a short copy.
                return finish_short(copied);
            }
            match fault_in_remote_page(self, VirtAddr::new(cur), write) {
                Ok(()) => {
                    faulted_at = Some(cur);
                    // Retry this page under a fresh read lock.
                }
                Err(_) => return finish_short(copied),
            }
        }
        Ok(copied)
    }
}

/// Common short-copy exit: return the copied count on progress, EIO if the first byte failed.
#[inline]
fn finish_short(copied: usize) -> Result<usize, SystemError> {
    if copied > 0 {
        Ok(copied)
    } else {
        Err(SystemError::EIO)
    }
}

/// Per-page VMA permission check under the read lock.
fn vma_permits(vma: &Arc<LockedVMA>, write: bool, force: bool) -> bool {
    let vm_flags = *vma.lock().vm_flags();
    if write {
        // Forced write only bypasses write-protection (COW of read-only private
        // mappings), not shared ones; private mappings without VM_WRITE pass as COW.
        vm_flags.contains(VmFlags::VM_WRITE)
            || (force
                && vm_flags.contains(VmFlags::VM_MAYWRITE)
                && !vm_flags.contains(VmFlags::VM_SHARED))
    } else {
        vm_flags.contains(VmFlags::VM_READ) || (force && vm_flags.contains(VmFlags::VM_MAYREAD))
    }
}
/// Page-cache location needed for dirty publish: cache, page and its index.
type PinnedDirtyPage = (Arc<PageCache>, Arc<Page>, usize);

/// For a present writable file-mapped page, resolve and pin the page-cache page to dirty.
fn pin_file_page_for_dirty(
    vma: &Arc<LockedVMA>,
    addr: VirtAddr,
    mapped_frame: &PhysAddr,
) -> Result<Option<PinnedDirtyPage>, SystemError> {
    let (page_cache, page_index) = {
        let vma_guard = vma.lock();
        let Some(file) = vma_guard.vm_file() else {
            // Not file-backed (anonymous etc.): plain copy, no dirty publish.
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
        // No page cache (device mappings etc.): plain copy.
        return Ok(None);
    };
    // Locate the managed page for this offset and confirm it maps the same physical frame.
    let Some(page) = page_cache.manager().get_page_any(page_index) else {
        return Ok(None);
    };
    let page_type = page.read().page_type().clone();
    let PageType::File(info) = page_type else {
        return Ok(None);
    };
    if page.phys_address() != *mapped_frame {
        // Private COW copy or index reclaimed: don't dirty the cache, plain copy.
        return Ok(None);
    }
    let Some(page_cache) = info.page_cache.upgrade() else {
        return Ok(None);
    };
    Ok(Some((page_cache, page, page_index)))
}

/// Publish page-cache dirtiness after the copy (index resolved earlier).
fn publish_pinned_page_dirty(
    page_cache: &Arc<PageCache>,
    page: &Arc<Page>,
    page_index: usize,
) -> Result<(), SystemError> {
    let mut dirty_reservation = page_cache.prepare_page_dirty()?;
    // Hold the page lock across the dirty publish: writeback completion samples
    // PG_DIRTY under the same lock, so this write can't merge into an older writeback round.
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

/// Fault in one page in the target address space (read lock must be released first).
fn fault_in_remote_page(
    target_vm: &Arc<AddressSpace>,
    address: VirtAddr,
    write: bool,
) -> Result<(), SystemError> {
    let mut flags = fault::FaultFlags::FAULT_FLAG_REMOTE;
    if write {
        flags |= fault::FaultFlags::FAULT_FLAG_WRITE;
    }

    // Pre-create the page cache for file-backed VMAs so the fault handler hits it.
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

    // Unified fault handling.
    let mut space_guard = target_vm.write();
    let Some(vma) = space_guard.mappings.find_nearest(address) else {
        return Err(SystemError::EIO);
    };

    let vma_guard = vma.lock();
    let region = *vma_guard.region();
    let vm_flags = *vma_guard.vm_flags();
    drop(vma_guard);

    if !region.contains(address) {
        // Address falls outside the nearest VMA
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

/// Pre-create the page-cache page of a file-backed VMA.
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
