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
use crate::filesystem::vfs::iov::IoVec;
use crate::mm::{access_ok, KernelWpGuard, MemoryManagementArch, PhysAddr, VirtAddr};
use crate::process::cred::CAPFlags;
use crate::process::{ProcessControlBlock, ProcessManager, RawPid};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;

/// Maximum number of iovec entries allowed (Linux default is 1024)
const UIO_MAXIOV: usize = 1024;

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

/// Check if current process has permission to access target process's memory
///
/// This implements a simplified version of Linux's ptrace_may_access() check.
/// Access is allowed if:
/// 1. Target is the same process as current (self-access)
/// 2. Current process has CAP_SYS_PTRACE capability
/// 3. Current process's uid/gid match target's euid/suid/uid and egid/sgid/gid
///
/// See Linux kernel: kernel/ptrace.c __ptrace_may_access()
fn check_process_vm_access(target_pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    let current_pcb = ProcessManager::current_pcb();

    // Self-access is always allowed
    if Arc::ptr_eq(&current_pcb, target_pcb) {
        return Ok(());
    }

    let current_cred = current_pcb.cred();
    let target_cred = target_pcb.cred();

    // CAP_SYS_PTRACE allows access to any process
    if current_cred.has_capability(CAPFlags::CAP_SYS_PTRACE) {
        return Ok(());
    }

    // Check uid/gid match (using real uid/gid as per PTRACE_MODE_REALCREDS)
    // All of target's uid variants must match current's uid
    // All of target's gid variants must match current's gid
    let uid_match = current_cred.uid == target_cred.euid
        && current_cred.uid == target_cred.suid
        && current_cred.uid == target_cred.uid;

    let gid_match = current_cred.gid == target_cred.egid
        && current_cred.gid == target_cred.sgid
        && current_cred.gid == target_cred.gid;

    if uid_match && gid_match {
        return Ok(());
    }

    // Permission denied - map to EPERM as Linux does for process_vm_* syscalls
    Err(SystemError::EPERM)
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

        // Read from remote process's address space
        let target_vm_guard = target_vm.read_irqsave();

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

    // Read from local process and write to target process
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

        // Write to remote process's address space
        let target_vm_guard = target_vm.read_irqsave();

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

        // Copy from local virtual address to remote physical address
        // Use exception-protected copy for safety
        unsafe {
            let remote_virt = MMArch::phys_2_virt(remote_phys).ok_or(SystemError::EFAULT)?;
            let src_ptr = local_addr.data() as *const u8;
            let dst_ptr = remote_virt.data() as *mut u8;

            // Use exception-protected copy
            let copy_result = MMArch::copy_with_exception_table(dst_ptr, src_ptr, actual_chunk);

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

syscall_table_macros::declare_syscall!(SYS_PROCESS_VM_READV, SysProcessVmReadvHandle);
syscall_table_macros::declare_syscall!(SYS_PROCESS_VM_WRITEV, SysProcessVmWritevHandle);
