//! Implementation of process_vm_readv and process_vm_writev syscalls
//!
//! These syscalls allow reading/writing data between the address space of the
//! calling process and another process.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::min;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::{SYS_PROCESS_VM_READV, SYS_PROCESS_VM_WRITEV};
use crate::arch::MMArch;
use crate::filesystem::page_cache::{PageCache, PageCachePagePin};
use crate::filesystem::vfs::iov::IoVec;
use crate::mm::{
    access_ok,
    fault::{FaultFlags, PageFaultHandler, PageFaultMessage},
    page::{page_manager_lock, PageFlags, PageType},
    KernelWpGuard, MemoryManagementArch, PhysAddr, VirtAddr, VirtRegion, VmFaultReason, VmFlags,
};
use crate::process::{ProcessControlBlock, ProcessManager, RawPid};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;

/// Maximum number of iovec entries allowed (Linux default is 1024)
const UIO_MAXIOV: usize = 1024;

struct RemoteWriteTarget {
    page: Arc<crate::mm::page::Page>,
    page_offset: usize,
    count: usize,
    file_page: Option<(Arc<PageCache>, usize, PageCachePagePin)>,
}

pub struct SysProcessVmReadvHandle;
pub struct SysProcessVmWritevHandle;

impl Syscall for SysProcessVmReadvHandle {
    fn num_args(&self) -> usize {
        6
    }

    /// process_vm_readv system call
    ///
    /// Reads data from another process's address space into local buffers.
    ///
    /// # Arguments (from args array)
    /// * `pid` - PID of the target process
    /// * `local_iov` - Pointer to local iovec array (destination buffers)
    /// * `liovcnt` - Number of local iovec entries
    /// * `remote_iov` - Pointer to remote iovec array (source buffers)
    /// * `riovcnt` - Number of remote iovec entries
    /// * `flags` - Flags (must be 0)
    ///
    /// # Returns
    /// Number of bytes read on success, or error
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0];
        let local_iov = args[1] as *const IoVec;
        let liovcnt = args[2];
        let remote_iov = args[3] as *const IoVec;
        let riovcnt = args[4];
        let flags = args[5];

        do_process_vm_readv(pid, local_iov, liovcnt, remote_iov, riovcnt, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{}", args[0] as i32)),
            FormattedSyscallParam::new("local_iov", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("liovcnt", format!("{}", args[2])),
            FormattedSyscallParam::new("remote_iov", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("riovcnt", format!("{}", args[4])),
            FormattedSyscallParam::new("flags", format!("{}", args[5])),
        ]
    }
}

impl Syscall for SysProcessVmWritevHandle {
    fn num_args(&self) -> usize {
        6
    }

    /// process_vm_writev system call
    ///
    /// Writes data from local buffers to another process's address space.
    ///
    /// # Arguments (from args array)
    /// * `pid` - PID of the target process
    /// * `local_iov` - Pointer to local iovec array (source buffers)
    /// * `liovcnt` - Number of local iovec entries
    /// * `remote_iov` - Pointer to remote iovec array (destination buffers)
    /// * `riovcnt` - Number of remote iovec entries
    /// * `flags` - Flags (must be 0)
    ///
    /// # Returns
    /// Number of bytes written on success, or error
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0];
        let local_iov = args[1] as *const IoVec;
        let liovcnt = args[2];
        let remote_iov = args[3] as *const IoVec;
        let riovcnt = args[4];
        let flags = args[5];

        do_process_vm_writev(pid, local_iov, liovcnt, remote_iov, riovcnt, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{}", args[0] as i32)),
            FormattedSyscallParam::new("local_iov", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("liovcnt", format!("{}", args[2])),
            FormattedSyscallParam::new("remote_iov", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("riovcnt", format!("{}", args[4])),
            FormattedSyscallParam::new("flags", format!("{}", args[5])),
        ]
    }
}

/// Validate iovec count and flags
fn validate_args(liovcnt: usize, riovcnt: usize, flags: usize) -> Result<(), SystemError> {
    // Flags must be 0
    if flags != 0 {
        return Err(SystemError::EINVAL);
    }

    // Check iovec count limits
    if liovcnt > UIO_MAXIOV || riovcnt > UIO_MAXIOV {
        return Err(SystemError::EINVAL);
    }

    Ok(())
}

/// Find target process by PID
fn find_target_process(pid: usize) -> Result<Arc<ProcessControlBlock>, SystemError> {
    if pid == 0 {
        return Err(SystemError::ESRCH);
    }

    let target_pcb =
        ProcessManager::find_task_by_vpid(RawPid::new(pid)).ok_or(SystemError::ESRCH)?;

    // Check if process is a zombie (no address space)
    if target_pcb.basic().user_vm().is_none() {
        return Err(SystemError::ESRCH);
    }

    Ok(target_pcb)
}

/// 检查当前进程是否有权访问目标进程内存。
pub fn check_process_vm_access(target_pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    let current_pcb = ProcessManager::current_pcb();
    if !current_pcb.has_permission_to_trace(target_pcb) {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

/// Read iovec array from user space
fn read_iovecs(iov_ptr: *const IoVec, iovcnt: usize) -> Result<Vec<IoVec>, SystemError> {
    if iovcnt == 0 {
        return Ok(Vec::new());
    }

    if iov_ptr.is_null() {
        return Err(SystemError::EFAULT);
    }

    let iov_size = iovcnt * core::mem::size_of::<IoVec>();
    let reader = UserBufferReader::new(iov_ptr, iov_size, true)?;
    let iovecs = reader.read_from_user::<IoVec>(0)?;

    Ok(iovecs.to_vec())
}

/// Calculate total length of iovec array with overflow checking
fn total_iov_len(iovecs: &[IoVec]) -> Result<usize, SystemError> {
    let mut total = 0usize;
    for iov in iovecs {
        total = total.checked_add(iov.iov_len).ok_or(SystemError::EINVAL)?;
    }
    Ok(total)
}

/// process_vm_readv implementation
///
/// Copies data from remote process to local process
fn do_process_vm_readv(
    pid: usize,
    local_iov: *const IoVec,
    liovcnt: usize,
    remote_iov: *const IoVec,
    riovcnt: usize,
    flags: usize,
) -> Result<usize, SystemError> {
    validate_args(liovcnt, riovcnt, flags)?;

    // Handle zero-length cases early
    if liovcnt == 0 || riovcnt == 0 {
        return Ok(0);
    }

    // Find target process first (before reading iovecs)
    // This ensures we return ESRCH for non-existent processes
    let target_pcb = find_target_process(pid)?;

    // Check permission to access target process's memory
    check_process_vm_access(&target_pcb)?;

    // Get target process's address space
    let target_vm = target_pcb.basic().user_vm().ok_or(SystemError::ESRCH)?;

    // Read local and remote iovec arrays
    let local_iovecs = read_iovecs(local_iov, liovcnt)?;
    let remote_iovecs = read_iovecs(remote_iov, riovcnt)?;

    // Calculate total lengths (with overflow checking)
    let local_len = total_iov_len(&local_iovecs)?;
    let remote_len = total_iov_len(&remote_iovecs)?;

    if local_len == 0 || remote_len == 0 {
        return Ok(0);
    }

    // Determine how much data to transfer
    let transfer_len = min(local_len, remote_len);

    // Read from target process and write to local process
    let mut bytes_copied = 0usize;
    let mut local_idx = 0usize;
    let mut local_offset = 0usize;
    let mut remote_idx = 0usize;
    let mut remote_offset = 0usize;

    while bytes_copied < transfer_len
        && local_idx < local_iovecs.len()
        && remote_idx < remote_iovecs.len()
    {
        let local_iov = &local_iovecs[local_idx];
        let remote_iov = &remote_iovecs[remote_idx];

        let local_remaining = local_iov.iov_len - local_offset;
        let remote_remaining = remote_iov.iov_len - remote_offset;

        if local_remaining == 0 {
            local_idx += 1;
            local_offset = 0;
            continue;
        }

        if remote_remaining == 0 {
            remote_idx += 1;
            remote_offset = 0;
            continue;
        }

        let chunk_len = min(local_remaining, remote_remaining);
        let chunk_len = min(chunk_len, transfer_len - bytes_copied);

        if chunk_len == 0 {
            break;
        }

        let local_addr = VirtAddr::new(local_iov.iov_base as usize + local_offset);
        let remote_addr = VirtAddr::new(remote_iov.iov_base as usize + remote_offset);

        // Verify local buffer is writable
        if access_ok(local_addr, chunk_len).is_err() {
            if bytes_copied > 0 {
                return Ok(bytes_copied);
            }
            return Err(SystemError::EFAULT);
        }

        let remote_page = VirtAddr::new(remote_addr.data() & !MMArch::PAGE_OFFSET_MASK);
        // Read from remote process's address space
        let target_vm_guard = target_vm
            .read_guard_no_reservation_conflict(VirtRegion::new(remote_page, MMArch::PAGE_SIZE));

        // Check if remote address is valid in target's address space
        if target_vm_guard.mappings.contains(remote_addr).is_none() {
            drop(target_vm_guard);
            if bytes_copied > 0 {
                return Ok(bytes_copied);
            }
            return Err(SystemError::EFAULT);
        }

        // Calculate page offset for this address
        let page_offset = remote_addr.data() & (MMArch::PAGE_SIZE - 1);

        // Translate remote virtual address to physical address
        // Note: translate() returns the page frame base, we need to add the offset
        let remote_phys = match target_vm_guard.user_mapper.utable.translate(remote_addr) {
            Some((phys_frame, _)) => PhysAddr::new(phys_frame.data() + page_offset),
            None => {
                drop(target_vm_guard);
                if bytes_copied > 0 {
                    return Ok(bytes_copied);
                }
                return Err(SystemError::EFAULT);
            }
        };
        drop(target_vm_guard);

        // Calculate how much we can copy in this iteration (don't cross page boundary)
        let max_in_page = MMArch::PAGE_SIZE - page_offset;
        let actual_chunk = min(chunk_len, max_in_page);

        // Copy from remote physical address to local virtual address
        // Note: We need to disable kernel write protection to write to user space
        // and use exception-protected copy for safety
        unsafe {
            let remote_virt = MMArch::phys_2_virt(remote_phys).ok_or(SystemError::EFAULT)?;
            let src_ptr = remote_virt.data() as *const u8;
            let dst_ptr = local_addr.data() as *mut u8;

            // Use RAII guard to ensure write protection is re-enabled even on panic
            let _wp_guard = KernelWpGuard::new();
            let copy_result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, actual_chunk);
            // _wp_guard dropped here, re-enabling write protection

            // If copy failed, return partial result or error
            if copy_result != 0 {
                if bytes_copied > 0 {
                    return Ok(bytes_copied);
                }
                return Err(SystemError::EFAULT);
            }
        }

        bytes_copied += actual_chunk;
        local_offset += actual_chunk;
        remote_offset += actual_chunk;

        if local_offset >= local_iov.iov_len {
            local_idx += 1;
            local_offset = 0;
        }
        if remote_offset >= remote_iov.iov_len {
            remote_idx += 1;
            remote_offset = 0;
        }
    }

    Ok(bytes_copied)
}

/// process_vm_writev implementation
///
/// Copies data from local process to remote process
fn do_process_vm_writev(
    pid: usize,
    local_iov: *const IoVec,
    liovcnt: usize,
    remote_iov: *const IoVec,
    riovcnt: usize,
    flags: usize,
) -> Result<usize, SystemError> {
    validate_args(liovcnt, riovcnt, flags)?;

    // Handle zero-length cases early
    if liovcnt == 0 || riovcnt == 0 {
        return Ok(0);
    }

    // Find target process first (before reading iovecs)
    // This ensures we return ESRCH for non-existent processes
    let target_pcb = find_target_process(pid)?;

    // Check permission to access target process's memory
    check_process_vm_access(&target_pcb)?;

    // Get target process's address space
    let target_vm = target_pcb.basic().user_vm().ok_or(SystemError::ESRCH)?;

    // Read local and remote iovec arrays
    let local_iovecs = read_iovecs(local_iov, liovcnt)?;
    let remote_iovecs = read_iovecs(remote_iov, riovcnt)?;

    // Calculate total lengths (with overflow checking)
    let local_len = total_iov_len(&local_iovecs)?;
    let remote_len = total_iov_len(&remote_iovecs)?;

    if local_len == 0 || remote_len == 0 {
        return Ok(0);
    }

    // Determine how much data to transfer
    let transfer_len = min(local_len, remote_len);

    // Stage at most one page before taking the target mm write lock. This is
    // required for self-process writes: faulting the local source while the
    // same address space is write-locked would deadlock.
    let mut staging = Vec::new();
    staging
        .try_reserve_exact(MMArch::PAGE_SIZE)
        .map_err(|_| SystemError::ENOMEM)?;
    staging.resize(MMArch::PAGE_SIZE, 0);

    // Read from local process and write to target process.
    let mut bytes_copied = 0usize;
    let mut local_idx = 0usize;
    let mut local_offset = 0usize;
    let mut remote_idx = 0usize;
    let mut remote_offset = 0usize;

    while bytes_copied < transfer_len
        && local_idx < local_iovecs.len()
        && remote_idx < remote_iovecs.len()
    {
        let local_iov = &local_iovecs[local_idx];
        let remote_iov = &remote_iovecs[remote_idx];

        let local_remaining = local_iov.iov_len - local_offset;
        let remote_remaining = remote_iov.iov_len - remote_offset;

        if local_remaining == 0 {
            local_idx += 1;
            local_offset = 0;
            continue;
        }

        if remote_remaining == 0 {
            remote_idx += 1;
            remote_offset = 0;
            continue;
        }

        let chunk_len = min(local_remaining, remote_remaining);
        let chunk_len = min(chunk_len, transfer_len - bytes_copied);

        if chunk_len == 0 {
            break;
        }

        let local_addr = VirtAddr::new(local_iov.iov_base as usize + local_offset);
        let remote_addr = VirtAddr::new(remote_iov.iov_base as usize + remote_offset);

        // Verify local buffer is readable
        if access_ok(local_addr, chunk_len).is_err() {
            if bytes_copied > 0 {
                return Ok(bytes_copied);
            }
            return Err(SystemError::EFAULT);
        }

        // Calculate how much we can copy in this iteration (don't cross a
        // remote page boundary). The target helper additionally clips at a VMA
        // boundary so adjacent mappings are revalidated independently.
        let page_offset = remote_addr.data() & (MMArch::PAGE_SIZE - 1);
        let max_in_page = MMArch::PAGE_SIZE - page_offset;
        let actual_chunk = min(chunk_len, max_in_page);

        // Copy the local source before taking target_vm.write().
        let copy_result = unsafe {
            MMArch::copy_with_exception_table(
                staging.as_mut_ptr(),
                local_addr.data() as *const u8,
                actual_chunk,
            )
        };
        if copy_result != 0 {
            if bytes_copied > 0 {
                return Ok(bytes_copied);
            }
            return Err(SystemError::EFAULT);
        }

        let written = match write_remote_page(&target_vm, remote_addr, &staging[..actual_chunk]) {
            Ok(written) => written,
            Err(_) if bytes_copied > 0 => return Ok(bytes_copied),
            Err(error) => return Err(error),
        };

        bytes_copied += written;
        local_offset += written;
        remote_offset += written;

        if local_offset >= local_iov.iov_len {
            local_idx += 1;
            local_offset = 0;
        }
        if remote_offset >= remote_iov.iov_len {
            remote_idx += 1;
            remote_offset = 0;
        }
    }

    Ok(bytes_copied)
}

/// Acquire and write one remote user page with Linux `FOLL_WRITE`-like
/// semantics. Anonymous/private pages remain write-locked through the copy so
/// fork cannot share their COW frame in between. Shared file pages instead use
/// a managed page-cache pin across the lockless dirty-and-copy phase.
fn write_remote_page(
    target_vm: &Arc<crate::mm::ucontext::AddressSpace>,
    remote_addr: VirtAddr,
    source: &[u8],
) -> Result<usize, SystemError> {
    let page_addr = VirtAddr::new(remote_addr.data() & !MMArch::PAGE_OFFSET_MASK);
    let page_offset = remote_addr.data() & MMArch::PAGE_OFFSET_MASK;
    let mut retried = false;
    let mut write_faults = 0u8;

    loop {
        let (retry_wait, target) = 'locked: {
            let mut guard = target_vm.write();
            let vma = guard
                .mappings
                .contains(remote_addr)
                .ok_or(SystemError::EFAULT)?;
            let vma_guard = vma.lock();
            let vm_flags = *vma_guard.vm_flags();
            if !vm_flags.contains(VmFlags::VM_WRITE)
                || vm_flags.intersects(VmFlags::VM_IO | VmFlags::VM_PFNMAP)
            {
                return Err(SystemError::EFAULT);
            }
            let is_shared_file =
                vm_flags.contains(VmFlags::VM_SHARED) && vma_guard.vm_file().is_some();
            let count = source
                .len()
                .min(MMArch::PAGE_SIZE - page_offset)
                .min(vma_guard.region().end().data() - remote_addr.data());
            drop(vma_guard);

            let needs_write_fault = guard
                .user_mapper
                .utable
                .translate(page_addr)
                .is_none_or(|(_, flags)| !flags.has_write());
            if needs_write_fault {
                let mut fault_flags = FaultFlags::FAULT_FLAG_WRITE
                    | FaultFlags::FAULT_FLAG_REMOTE
                    | FaultFlags::FAULT_FLAG_ALLOW_RETRY
                    | FaultFlags::FAULT_FLAG_KILLABLE;
                if retried {
                    fault_flags |= FaultFlags::FAULT_FLAG_TRIED;
                }
                let outcome = unsafe {
                    PageFaultHandler::handle_mm_fault(PageFaultMessage::new(
                        vma,
                        page_addr,
                        fault_flags,
                        &mut guard.user_mapper.utable,
                        target_vm.clone(),
                    ))
                };
                if outcome.reason.contains(VmFaultReason::VM_FAULT_RETRY) {
                    break 'locked (outcome.retry_wait, None);
                }
                if outcome.reason.intersects(VmFaultReason::VM_FAULT_ERROR) {
                    return if outcome.reason.contains(VmFaultReason::VM_FAULT_OOM) {
                        Err(SystemError::ENOMEM)
                    } else {
                        Err(SystemError::EFAULT)
                    };
                }

                let (_, entry_flags) = guard
                    .user_mapper
                    .utable
                    .translate(page_addr)
                    .ok_or(SystemError::EFAULT)?;
                if !entry_flags.has_write() {
                    // DragonOS currently resolves a missing private mapping in
                    // two steps: instantiate the read-only COW page, then
                    // handle its write protection. Mirror the hardware fault
                    // loop instead of writing through a read-only PTE.
                    write_faults += 1;
                    if write_faults <= 2 {
                        break 'locked (None, None);
                    }
                    return Err(SystemError::EFAULT);
                }
            }

            let (paddr, entry_flags) = guard
                .user_mapper
                .utable
                .translate(page_addr)
                .ok_or(SystemError::EFAULT)?;
            debug_assert!(entry_flags.has_write());
            let page = page_manager_lock().get(&paddr).ok_or(SystemError::EFAULT)?;
            if !is_shared_file {
                // Keep the mm write lock through anonymous/private-COW writes.
                // Besides pinning the frame, this prevents fork from sharing
                // the page between the write fault and the physical copy.
                copy_staged_to_remote(page.phys_address(), page_offset, source, count)?;
                return Ok(count);
            }
            break 'locked (
                None,
                Some(RemoteWriteTarget {
                    page,
                    page_offset,
                    count,
                    file_page: None,
                }),
            );
        };

        if let Some(mut target) = target {
            // Never take a page lock while holding the target mm lock:
            // writeback takes the inverse page -> mm order while cleaning
            // reverse mappings. The managed Page Arc above pins the frame
            // across this gap, just as Linux pins a GUP result before copy.
            let page_type = { target.page.read().page_type().clone() };
            target.file_page = match page_type {
                PageType::File(info) => {
                    let cache = info.page_cache.upgrade().ok_or(SystemError::EFAULT)?;
                    let pin = cache
                        .get_ready_page_pinned(info.index)
                        .ok_or(SystemError::EFAULT)?;
                    if !Arc::ptr_eq(&pin.page(), &target.page) {
                        return Err(SystemError::EFAULT);
                    }
                    Some((cache, info.index, pin))
                }
                _ => return Err(SystemError::EFAULT),
            };
            let count = target.count;
            write_remote_target(target, source)?;
            return Ok(count);
        }
        if let Some(wait) = retry_wait {
            wait.wait()?;
        }
        retried = true;
    }
}

fn write_remote_target(target: RemoteWriteTarget, source: &[u8]) -> Result<(), SystemError> {
    let RemoteWriteTarget {
        page,
        page_offset,
        count,
        file_page,
    } = target;
    let copy = || copy_staged_to_remote(page.phys_address(), page_offset, source, count);
    let Some((cache, index, _pin)) = file_page else {
        return copy();
    };

    // Writeback takes the page lock before walking reverse mappings. The mm
    // lock was deliberately released above, matching Linux's GUP-then-copy
    // order and avoiding an mm.write -> page.write -> mm.read ABBA cycle.
    let mut reservation = cache.prepare_page_dirty()?;
    let mut page_guard = page.write();
    page_guard.add_flags(PageFlags::PG_DIRTY);
    let publication = cache.mark_page_dirty_prepared_page_locked_with_transition(
        index,
        &mut reservation,
        &page_guard,
    );
    match publication {
        Ok(Some(_)) => copy(),
        Ok(None) => {
            page_guard.remove_flags(PageFlags::PG_DIRTY);
            Err(SystemError::EFAULT)
        }
        Err(error) => {
            page_guard.remove_flags(PageFlags::PG_DIRTY);
            Err(error)
        }
    }
}

fn copy_staged_to_remote(
    page_paddr: PhysAddr,
    page_offset: usize,
    source: &[u8],
    count: usize,
) -> Result<(), SystemError> {
    let remote_phys = PhysAddr::new(page_paddr.data() + page_offset);
    let remote_virt = unsafe { MMArch::phys_2_virt(remote_phys).ok_or(SystemError::EFAULT)? };
    let not_copied = unsafe {
        MMArch::copy_with_exception_table(remote_virt.data() as *mut u8, source.as_ptr(), count)
    };
    if not_copied != 0 {
        return Err(SystemError::EFAULT);
    }
    Ok(())
}

syscall_table_macros::declare_syscall!(SYS_PROCESS_VM_READV, SysProcessVmReadvHandle);
syscall_table_macros::declare_syscall!(SYS_PROCESS_VM_WRITEV, SysProcessVmWritevHandle);
