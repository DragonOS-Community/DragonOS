//! Unified entry for remote access to user memory across address spaces.
//!
//! Managed pages are pinned by `Arc<Page>` while the target mm and page-table
//! edit locks make the PTE identity stable. Both mm guards are released before
//! copying. A remote write admits any exact PageCache dirty lifetime before it
//! changes bytes and publishes its generation while holding the Page lock;
//! writeback cannot snapshot that generation until the infallible copy and
//! `PG_DIRTY` update finish.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::page_cache::{PageCache, PreparedRemotePageDirty},
    mm::{
        fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
        page::{page_manager_lock, Page, PageFlags},
        ucontext::{AddressSpace, LockedVMA},
        MemoryManagementArch, PhysAddr, VirtAddr, VmFaultReason, VmFlags,
    },
};

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

type PageCacheEntry = (Arc<PageCache>, usize);

struct PinnedRemotePage {
    page: Arc<Page>,
    frame: PhysAddr,
    cache_entry: Option<PageCacheEntry>,
}

enum PinRemoteError {
    Fault,
    Denied,
    UnsupportedSpecial,
}

impl AddressSpace {
    /// Access user memory across address spaces.
    ///
    /// `force=true` implements ptrace and `/proc/[pid]/mem` permission
    /// semantics. `force=false` implements `process_vm_*` permissions.
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
        if write && force {
            self.prepare_remote_user_write()?;
        }
        let finish = |copied| {
            if copied != 0 && write && force {
                self.sync_remote_user_icache();
            }
            finish_short(copied)
        };

        let mut copied = 0usize;
        while copied < total {
            let current = match addr.checked_add(copied) {
                Some(current) if current < MMArch::USER_END_VADDR.data() => current,
                _ => return finish(copied),
            };
            let page_offset = current & (MMArch::PAGE_SIZE - 1);
            let chunk = core::cmp::min(total - copied, MMArch::PAGE_SIZE - page_offset);
            let page_addr = VirtAddr::new(current - page_offset);

            let pinned = match pin_remote_page(self, VirtAddr::new(current), write, force) {
                Ok(pinned) => pinned,
                Err(PinRemoteError::Denied | PinRemoteError::UnsupportedSpecial) => {
                    return finish(copied)
                }
                Err(PinRemoteError::Fault) => {
                    match fault_in_remote_page(self, VirtAddr::new(current), write, force) {
                        Ok(pinned) => pinned,
                        Err(_) => return finish(copied),
                    }
                }
            };

            let dirty = if write {
                match pinned.cache_entry.as_ref() {
                    Some((cache, index)) => {
                        match cache.prepare_remote_page_dirty(*index, &pinned.page) {
                            Ok(token) => token,
                            Err(_) => return finish(copied),
                        }
                    }
                    None => None,
                }
            } else {
                None
            };

            copy_pinned_page(&mut dir, copied, page_offset, chunk, &pinned.page, dirty);
            if write {
                mark_remote_pte_dirty(self, page_addr, pinned.frame);
            }
            copied += chunk;
        }
        finish(copied)
    }
}

fn pin_remote_page(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    write: bool,
    force: bool,
) -> Result<PinnedRemotePage, PinRemoteError> {
    let guard = mm.read();
    let vma = guard
        .mappings
        .contains(address)
        .ok_or(PinRemoteError::Fault)?;
    let (vm_flags, cache_entry) = remote_vma_snapshot(&vma, address, write)?;
    if !vma_permits(vm_flags, write, force) {
        return Err(PinRemoteError::Denied);
    }
    if vm_flags.intersects(VmFlags::VM_IO | VmFlags::VM_PFNMAP) {
        return Err(PinRemoteError::UnsupportedSpecial);
    }

    let page_addr = VirtAddr::new(address.data() & !(MMArch::PAGE_SIZE - 1));
    let _page_table_edit = mm.page_table_edit();
    let (frame, flags) = guard
        .user_mapper
        .utable
        .translate(page_addr)
        .ok_or(PinRemoteError::Fault)?;
    if !flags.has_user() && !force {
        return Err(PinRemoteError::Denied);
    }
    let page = page_manager_lock()
        .get(&frame)
        .ok_or(PinRemoteError::UnsupportedSpecial)?;
    if write && !flags.has_write() {
        return Err(PinRemoteError::Fault);
    }
    let (validated_frame, _) = guard
        .user_mapper
        .utable
        .translate(page_addr)
        .ok_or(PinRemoteError::Fault)?;
    if validated_frame != frame || page.phys_address() != frame {
        return Err(PinRemoteError::Fault);
    }

    Ok(PinnedRemotePage {
        page,
        frame,
        cache_entry,
    })
}

fn remote_vma_snapshot(
    vma: &Arc<LockedVMA>,
    address: VirtAddr,
    write: bool,
) -> Result<(VmFlags, Option<PageCacheEntry>), PinRemoteError> {
    let guard = vma.lock();
    let flags = *guard.vm_flags();
    if !write {
        return Ok((flags, None));
    }
    let Some(file) = guard.vm_file() else {
        return Ok((flags, None));
    };
    let Some(base_pgoff) = guard.backing_page_offset() else {
        return Err(PinRemoteError::Denied);
    };
    let relative = address
        .data()
        .checked_sub(guard.region().start().data())
        .ok_or(PinRemoteError::Fault)?;
    let index = base_pgoff
        .checked_add(relative >> MMArch::PAGE_SHIFT)
        .ok_or(PinRemoteError::Denied)?;
    Ok((flags, file.inode().page_cache().map(|cache| (cache, index))))
}

fn copy_pinned_page(
    direction: &mut RemoteAccess<'_>,
    buffer_offset: usize,
    page_offset: usize,
    len: usize,
    page: &Arc<Page>,
    mut dirty: Option<PreparedRemotePageDirty>,
) {
    match direction {
        RemoteAccess::Read(buffer) => {
            let page = page.read();
            let source = unsafe { &page.as_slice()[page_offset..page_offset + len] };
            buffer[buffer_offset..buffer_offset + len].copy_from_slice(source);
        }
        RemoteAccess::Write(buffer) => {
            let mut page = page.write();
            if let Some(dirty) = dirty.as_mut() {
                dirty.publish_before_copy(&page);
            }
            let target = unsafe { &mut page.as_slice_mut()[page_offset..page_offset + len] };
            target.copy_from_slice(&buffer[buffer_offset..buffer_offset + len]);
            page.add_flags(PageFlags::PG_DIRTY);
        }
    }
}

fn mark_remote_pte_dirty(mm: &Arc<AddressSpace>, address: VirtAddr, expected_frame: PhysAddr) {
    let guard = mm.read();
    let _page_table_edit = mm.page_table_edit();
    let Some((frame, flags)) = guard.user_mapper.utable.translate(address) else {
        return;
    };
    if frame != expected_frame || flags.has_flag(MMArch::ENTRY_FLAG_DIRTY) {
        return;
    }
    if let Some(flush) = unsafe {
        guard
            .user_mapper
            .utable
            .remap_present(address, flags.set_dirty(true))
    } {
        // Only the software dirty bit changed; stale TLB permissions and frame
        // identity remain valid, so a cross-CPU invalidation is unnecessary.
        unsafe { flush.ignore() };
    }
}

fn finish_short(copied: usize) -> Result<usize, SystemError> {
    if copied == 0 {
        Err(SystemError::EIO)
    } else {
        Ok(copied)
    }
}

fn vma_permits(flags: VmFlags, write: bool, force: bool) -> bool {
    if write {
        flags.contains(VmFlags::VM_WRITE)
            || (force
                && flags.contains(VmFlags::VM_MAYWRITE)
                && !flags.contains(VmFlags::VM_SHARED))
    } else {
        flags.contains(VmFlags::VM_READ) || (force && flags.contains(VmFlags::VM_MAYREAD))
    }
}

/// Fault one target page, honoring the handler's drop-lock-and-wait retry
/// protocol. No AddressSpace guard is held while a retry token blocks.
fn fault_in_remote_page(
    mm: &Arc<AddressSpace>,
    address: VirtAddr,
    write: bool,
    force: bool,
) -> Result<PinnedRemotePage, SystemError> {
    let mut tried = false;
    loop {
        let retry_wait = {
            let mut guard = mm.write();
            let vma = guard
                .mappings
                .find_nearest(address)
                .ok_or(SystemError::EFAULT)?;
            let (region, vm_flags) = {
                let vma = vma.lock();
                (*vma.region(), *vma.vm_flags())
            };
            if !region.contains(address) {
                if !vm_flags.contains(VmFlags::VM_GROWSDOWN) {
                    return Err(SystemError::EFAULT);
                }
                let extension = region
                    .start()
                    .data()
                    .checked_sub(address.data())
                    .ok_or(SystemError::EFAULT)?;
                let max_stack = guard
                    .user_stack
                    .as_ref()
                    .map(|stack| stack.max_limit())
                    .unwrap_or(0);
                if extension > max_stack || !guard.can_extend_stack(extension) {
                    return Err(SystemError::EFAULT);
                }
                let post_commit = guard.extend_stack(extension)?;
                drop(guard);
                if let Some(request) = post_commit {
                    mm.populate_locked_vma_post_commit(request)?;
                }
                tried = false;
                continue;
            }
            if !vma_permits(vm_flags, write, force) {
                return Err(SystemError::EFAULT);
            }
            if vm_flags.intersects(VmFlags::VM_IO | VmFlags::VM_PFNMAP) {
                return Err(SystemError::EFAULT);
            }

            let mut flags = FaultFlags::FAULT_FLAG_REMOTE
                | FaultFlags::FAULT_FLAG_ALLOW_RETRY
                | FaultFlags::FAULT_FLAG_KILLABLE;
            if write {
                flags |= FaultFlags::FAULT_FLAG_WRITE;
            }
            if tried {
                flags |= FaultFlags::FAULT_FLAG_TRIED;
            }
            let outcome = unsafe {
                let message = PageFaultMessage::new(
                    vma.clone(),
                    address,
                    flags,
                    &mut guard.user_mapper.utable,
                    mm.clone(),
                );
                PageFaultHandler::handle_mm_fault(message)
            };
            if outcome.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                let page_addr = VirtAddr::new(address.data() & !(MMArch::PAGE_SIZE - 1));
                let frame = guard
                    .user_mapper
                    .utable
                    .translate(page_addr)
                    .map(|(frame, _)| frame)
                    .ok_or(SystemError::EFAULT)?;
                let page = page_manager_lock().get(&frame).ok_or(SystemError::EFAULT)?;
                let (_, cache_entry) =
                    remote_vma_snapshot(&vma, address, write).map_err(|_| SystemError::EFAULT)?;
                return Ok(PinnedRemotePage {
                    page,
                    frame,
                    cache_entry,
                });
            }
            if outcome.reason.contains(VmFaultReason::VM_FAULT_OOM) {
                return Err(SystemError::ENOMEM);
            }
            if !outcome.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                return Err(SystemError::EFAULT);
            }
            outcome.retry_wait
        };

        if let Some(wait) = retry_wait {
            wait.wait()?;
        } else {
            crate::sched::sched_yield();
        }
        tried = true;
    }
}
