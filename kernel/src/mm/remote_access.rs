//! Unified entry for remote access to user memory across address spaces.
//!
//! Managed pages acquire an owned physical-access pin while the target mm and
//! page-table edit locks make the PTE identity stable. Both guards drop before
//! copying. A remote write admits any exact PageCache dirty lifetime before it
//! changes bytes and publishes its generation while holding the Page lock;
//! writeback cannot snapshot that generation until the infallible copy and
//! `PG_DIRTY` update finish.

use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::MMArch,
    filesystem::{
        page_cache::{PageCache, PreparedRemotePageDirty},
        vfs::IndexNode,
    },
    mm::{
        fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
        page::{
            access::{PageAccessError, PageAccessPin},
            page_manager_lock, Page, PageFlags,
        },
        ucontext::{AddressSpace, InnerAddressSpace, LockedVMA},
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

struct RemoteVmaSnapshot {
    flags: VmFlags,
    cache_entry: Option<PageCacheEntry>,
    writer_inode: Option<Arc<dyn IndexNode>>,
}

struct PinnedRemotePage {
    access: PageAccessPin,
    cache_entry: Option<PageCacheEntry>,
    _writer: Option<RemoteWriteReservation>,
}

struct RemoteWriteReservation(Arc<dyn IndexNode>);

impl Drop for RemoteWriteReservation {
    fn drop(&mut self) {
        self.0.end_remote_write();
    }
}

enum PinRemoteError {
    Fault,
    WriteProtect,
    Denied,
    UnsupportedSpecial,
    Busy(Arc<Page>),
    Overflow,
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
                Err(PinRemoteError::Fault | PinRemoteError::WriteProtect) => {
                    match fault_in_remote_page(self, VirtAddr::new(current), write, force) {
                        Ok(pinned) => pinned,
                        Err(SystemError::EOVERFLOW) if copied == 0 => {
                            return Err(SystemError::EOVERFLOW);
                        }
                        Err(_) => return finish(copied),
                    }
                }
                Err(PinRemoteError::Busy(page)) => {
                    // Isolation holds the Page write guard. No MM/PT/cache
                    // resource is retained while waiting, and the old Page
                    // is only a wait target: lookup must start over afterwards.
                    drop(page.write());
                    continue;
                }
                Err(PinRemoteError::Overflow) => {
                    return if copied == 0 {
                        Err(SystemError::EOVERFLOW)
                    } else {
                        finish(copied)
                    };
                }
            };

            let dirty = if write {
                match pinned.cache_entry.as_ref() {
                    Some((cache, index)) => {
                        match cache.prepare_remote_page_dirty(*index, pinned.access.page()) {
                            Ok(token) => token,
                            Err(_) => return finish(copied),
                        }
                    }
                    None => None,
                }
            } else {
                None
            };

            copy_pinned_page(&mut dir, copied, page_offset, chunk, &pinned.access, dirty);
            if write {
                mark_remote_pte_dirty(self, page_addr, pinned.access.phys_address());
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
    pin_remote_page_locked(mm, &guard, address, write, force, false)
}

/// Both normal lookup and completed faults admit the exact Page while their
/// caller retains the MM lock. A forced write may copy a COW-resolved page
/// whose VMA still intentionally lacks VM_WRITE (ptrace/proc mem semantics).
fn pin_remote_page_locked(
    mm: &Arc<AddressSpace>,
    space: &InnerAddressSpace,
    address: VirtAddr,
    write: bool,
    force: bool,
    fault_completed: bool,
) -> Result<PinnedRemotePage, PinRemoteError> {
    let vma = space
        .mappings
        .contains(address)
        .ok_or(PinRemoteError::Fault)?;
    let snapshot = remote_vma_snapshot(&vma, address, write)?;
    let vm_flags = snapshot.flags;
    if !vma_permits(vm_flags, write, force) {
        return Err(PinRemoteError::Denied);
    }
    if vm_flags.intersects(VmFlags::VM_IO | VmFlags::VM_PFNMAP) {
        return Err(PinRemoteError::UnsupportedSpecial);
    }

    let page_addr = VirtAddr::new(address.data() & !(MMArch::PAGE_SIZE - 1));
    let _page_table_edit = mm.page_table_edit();
    let (frame, flags) = space
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
    if write && !flags.has_write() && !(force && fault_completed) {
        return Err(PinRemoteError::WriteProtect);
    }
    let (validated_frame, _) = space
        .user_mapper
        .utable
        .translate(page_addr)
        .ok_or(PinRemoteError::Fault)?;
    if validated_frame != frame || page.phys_address() != frame {
        return Err(PinRemoteError::Fault);
    }
    let access = match page.try_pin_access() {
        Ok(pin) => pin,
        Err(PageAccessError::Busy) => return Err(PinRemoteError::Busy(page)),
        Err(PageAccessError::Overflow) => return Err(PinRemoteError::Overflow),
    };

    // The mm guard still protects the VMA here.  A writer reservation then
    // covers the interval after this guard drops and before the copy ends.
    let writer = snapshot.writer_inode.and_then(|inode| {
        inode
            .begin_remote_write()
            .then_some(RemoteWriteReservation(inode))
    });

    Ok(PinnedRemotePage {
        access,
        cache_entry: snapshot.cache_entry,
        _writer: writer,
    })
}

fn remote_vma_snapshot(
    vma: &Arc<LockedVMA>,
    address: VirtAddr,
    write: bool,
) -> Result<RemoteVmaSnapshot, PinRemoteError> {
    let guard = vma.lock();
    let flags = *guard.vm_flags();
    if !write {
        return Ok(RemoteVmaSnapshot {
            flags,
            cache_entry: None,
            writer_inode: None,
        });
    }
    let Some(file) = guard.vm_file() else {
        return Ok(RemoteVmaSnapshot {
            flags,
            cache_entry: None,
            writer_inode: None,
        });
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
    let inode = file.inode();
    let writer_inode = flags
        .contains(VmFlags::VM_SHARED | VmFlags::VM_WRITE)
        .then(|| inode.clone());
    Ok(RemoteVmaSnapshot {
        flags,
        cache_entry: inode.page_cache().map(|cache| (cache, index)),
        writer_inode,
    })
}

fn copy_pinned_page(
    direction: &mut RemoteAccess<'_>,
    buffer_offset: usize,
    page_offset: usize,
    len: usize,
    access: &PageAccessPin,
    mut dirty: Option<PreparedRemotePageDirty>,
) {
    let page = access.page();
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
    let mut charge_recovery = crate::cgroup::memory::runtime::ChargeRecoveryState::new();
    loop {
        let creator =
            crate::cgroup::memory::object_for_task(&crate::process::ProcessManager::current_pcb());
        let data_account = mm.memory_account();
        let (retry_wait, retry_flags) = {
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
                let post_commit = match guard.extend_stack(extension, Some(&creator), &data_account)
                {
                    Ok(request) => request,
                    Err(error) => {
                        drop(guard);
                        error.recover(&mut charge_recovery)?;
                        continue;
                    }
                };
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
                    creator.clone(),
                    data_account.clone(),
                );
                PageFaultHandler::handle_mm_fault(message)
            };
            if let Some(failure) = outcome.charge_failure {
                drop(guard);
                crate::mm::fault::recover_fault_charge(
                    failure,
                    &mut charge_recovery,
                    &data_account,
                    flags,
                )?;
                continue;
            }
            if outcome.reason.contains(VmFaultReason::VM_FAULT_COMPLETED) {
                match pin_remote_page_locked(mm, &guard, address, write, force, true) {
                    Ok(pinned) => return Ok(pinned),
                    Err(PinRemoteError::WriteProtect) => {
                        // An initial anonymous fault can install PAGE_COPY.
                        // Like GUP, follow again after fault completion and
                        // resolve its write protection before admitting a
                        // non-forced writer. A missing leaf is not this retry.
                        drop(guard);
                        crate::sched::cond_resched();
                        continue;
                    }
                    Err(PinRemoteError::Busy(page)) => {
                        drop(guard);
                        drop(page.write());
                        // Re-run fault/COW and lookup after waiting. In
                        // particular a forced write must not reuse a former
                        // COW result after fork changed its PTE identity.
                        tried = false;
                        continue;
                    }
                    Err(PinRemoteError::Overflow) => return Err(SystemError::EOVERFLOW),
                    Err(_) => return Err(SystemError::EFAULT),
                }
            }
            if outcome.reason.contains(VmFaultReason::VM_FAULT_OOM) {
                return Err(SystemError::ENOMEM);
            }
            if !outcome.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                return Err(SystemError::EFAULT);
            }
            (outcome.retry_wait, flags)
        };

        if let Some(wait) = retry_wait {
            let result = match wait.wait() {
                Err(crate::mm::allocation::MemoryAllocationError::Charge(failure)) => {
                    crate::mm::fault::recover_fault_charge(
                        failure,
                        &mut charge_recovery,
                        &data_account,
                        retry_flags,
                    )?;
                    tried = true;
                    continue;
                }
                Err(crate::mm::allocation::MemoryAllocationError::System(error)) => Err(error),
                Ok(()) => Ok(()),
            };
            if let Err(error) = result {
                if !matches!(error, SystemError::EINTR | SystemError::ERESTARTSYS) {
                    crate::mm::fault::account_fault_result(
                        &data_account,
                        retry_flags,
                        VmFaultReason::VM_FAULT_SIGBUS,
                    );
                }
                return Err(error);
            }
        } else {
            crate::sched::sched_yield();
        }
        tried = true;
    }
}
