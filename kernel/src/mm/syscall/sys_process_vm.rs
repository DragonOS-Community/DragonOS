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
use crate::mm::{access_ok, remote_access::RemoteAccess, MemoryManagementArch, VirtAddr};
use crate::process::{ptrace::PtraceAccessCreds, ProcessControlBlock, ProcessManager, RawPid};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};

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

        process_vm_rw(
            ProcessVmDirection::Read,
            pid,
            local_iov,
            liovcnt,
            remote_iov,
            riovcnt,
            flags,
        )
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

        process_vm_rw(
            ProcessVmDirection::Write,
            pid,
            local_iov,
            liovcnt,
            remote_iov,
            riovcnt,
            flags,
        )
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

/// Check whether the current process may access the target's memory.
fn check_process_vm_access(target_pcb: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
    let current_pcb = ProcessManager::current_pcb();
    if !current_pcb.has_permission_to_trace(target_pcb, PtraceAccessCreds::RealCreds) {
        return Err(SystemError::EPERM);
    }
    Ok(())
}

/// Copy an iovec array from userspace without touching the buffers it describes.
fn import_iovecs(iov_ptr: *const IoVec, iovcnt: usize) -> Result<Vec<IoVec>, SystemError> {
    if iovcnt > UIO_MAXIOV {
        return Err(SystemError::EINVAL);
    }
    if iovcnt == 0 {
        return Ok(Vec::new());
    }
    let iov_size = iovcnt
        .checked_mul(core::mem::size_of::<IoVec>())
        .ok_or(SystemError::EINVAL)?;
    let reader = UserBufferReader::new(iov_ptr, iov_size, true)?;
    let user_iovecs = reader.buffer_protected(0)?;
    let mut iovecs = Vec::new();
    iovecs
        .try_reserve_exact(iovcnt)
        .map_err(|_| SystemError::ENOMEM)?;
    for index in 0..iovcnt {
        let iovec: IoVec = user_iovecs.read_one(index * core::mem::size_of::<IoVec>())?;
        // Linux copy_iovec_from_user() imports iov_len through ssize_t and
        // rejects any size_t value whose sign bit is set before range checks.
        if iovec.iov_len > isize::MAX as usize {
            return Err(SystemError::EINVAL);
        }
        iovecs.push(iovec);
    }
    Ok(iovecs)
}

/// Match import_iovec(): validate local ranges and cap the iterator at MAX_RW_COUNT.
fn prepare_local_iovecs(iovecs: &mut [IoVec]) -> Result<usize, SystemError> {
    let max_rw_count = (i32::MAX as usize) & !(MMArch::PAGE_SIZE - 1);
    let mut total = 0usize;
    let single_segment = iovecs.len() == 1;
    for iov in iovecs.iter_mut() {
        // Linux import_iovec() validates every original range, including
        // segments which will later be truncated away by MAX_RW_COUNT.  Its
        // one-segment import_ubuf() fast path instead checks the capped size.
        let len = min(iov.iov_len, max_rw_count - total);
        let checked_len = if single_segment { len } else { iov.iov_len };
        access_ok(VirtAddr::new(iov.iov_base as usize), checked_len)
            .map_err(|_| SystemError::EFAULT)?;
        iov.iov_len = len;
        total += len;
    }
    Ok(total)
}

/// Common failure exit: return a short count on progress, EFAULT otherwise.
fn partial_or_fault(bytes_copied: usize) -> Result<usize, SystemError> {
    if bytes_copied > 0 {
        Ok(bytes_copied)
    } else {
        Err(SystemError::EFAULT)
    }
}

#[derive(Clone, Copy)]
enum ProcessVmDirection {
    Read,
    Write,
}

#[derive(Default)]
struct TransferCursor {
    local_index: usize,
    local_offset: usize,
    remote_index: usize,
    remote_offset: usize,
}

impl TransferCursor {
    fn skip_empty(&mut self, local_iovecs: &[IoVec], remote_iovecs: &[IoVec]) {
        while self.local_index < local_iovecs.len()
            && self.local_offset == local_iovecs[self.local_index].iov_len
        {
            self.local_index += 1;
            self.local_offset = 0;
        }
        while self.remote_index < remote_iovecs.len()
            && self.remote_offset == remote_iovecs[self.remote_index].iov_len
        {
            self.remote_index += 1;
            self.remote_offset = 0;
        }
    }

    fn advance(&mut self, copied: usize) {
        self.local_offset += copied;
        self.remote_offset += copied;
    }
}

fn process_vm_rw(
    direction: ProcessVmDirection,
    pid: usize,
    local_iov: *const IoVec,
    liovcnt: usize,
    remote_iov: *const IoVec,
    riovcnt: usize,
    flags: usize,
) -> Result<usize, SystemError> {
    if flags != 0 {
        return Err(SystemError::EINVAL);
    }

    // Linux imports local iovecs first and returns before even inspecting the
    // remote array or pid when the resulting local iterator is empty.
    let mut local_iovecs = import_iovecs(local_iov, liovcnt)?;
    let local_len = prepare_local_iovecs(&mut local_iovecs)?;
    if local_len == 0 {
        return Ok(0);
    }

    let remote_iovecs = import_iovecs(remote_iov, riovcnt)?;
    if !remote_iovecs.iter().any(|iov| iov.iov_len != 0) {
        return Ok(0);
    }

    // Linux admits its process-page scratch before pid lookup/mm_access. Keep
    // the same error priority for DragonOS's page-sized transfer scratch.
    let mut bounce = Vec::new();
    bounce
        .try_reserve_exact(MMArch::PAGE_SIZE)
        .map_err(|_| SystemError::ENOMEM)?;
    bounce.resize(MMArch::PAGE_SIZE, 0);

    let target_pcb = find_target_process(pid)?;
    let target_mm_guard = {
        let _exec_guard = target_pcb.exec_update_read();
        let guard = target_pcb.active_vm().ok_or(SystemError::ESRCH)?;
        check_process_vm_access(&target_pcb)?;
        guard
    };
    let target_vm = target_mm_guard.vm().clone();

    let mut bytes_copied = 0usize;
    let mut cursor = TransferCursor::default();
    while bytes_copied < local_len {
        cursor.skip_empty(&local_iovecs, &remote_iovecs);
        if cursor.local_index == local_iovecs.len() || cursor.remote_index == remote_iovecs.len() {
            break;
        }

        let local = &local_iovecs[cursor.local_index];
        let remote = &remote_iovecs[cursor.remote_index];
        let chunk_len = min(
            min(
                local.iov_len - cursor.local_offset,
                remote.iov_len - cursor.remote_offset,
            ),
            bounce.len(),
        );
        let Some(local_addr) = (local.iov_base as usize).checked_add(cursor.local_offset) else {
            return partial_or_fault(bytes_copied);
        };
        let Some(remote_addr) = (remote.iov_base as usize).checked_add(cursor.remote_offset) else {
            return partial_or_fault(bytes_copied);
        };
        // Linux advances its local iov_iter by the bytes copied before a user
        // fault.  DragonOS's protected user-copy helper is all-or-error, so
        // never let one helper call straddle a local page boundary: a hole in
        // the following page is then observed only after the preceding page's
        // progress has been committed to bytes_copied.
        let local_page_remaining = MMArch::PAGE_SIZE - (local_addr & (MMArch::PAGE_SIZE - 1));
        let chunk_len = min(chunk_len, local_page_remaining);

        let copied = match direction {
            ProcessVmDirection::Read => {
                let copied = match target_vm.access_remote_vm(
                    remote_addr,
                    RemoteAccess::Read(&mut bounce[..chunk_len]),
                    false,
                ) {
                    Ok(copied) if copied != 0 => copied,
                    Ok(_) | Err(_) => return partial_or_fault(bytes_copied),
                };
                let mut writer = match UserBufferWriter::new(local_addr as *mut u8, copied, true) {
                    Ok(writer) => writer,
                    Err(_) => return partial_or_fault(bytes_copied),
                };
                if writer.copy_to_user_protected(&bounce[..copied], 0).is_err() {
                    return partial_or_fault(bytes_copied);
                }
                copied
            }
            ProcessVmDirection::Write => {
                let reader = match UserBufferReader::new(local_addr as *const u8, chunk_len, true) {
                    Ok(reader) => reader,
                    Err(_) => return partial_or_fault(bytes_copied),
                };
                if reader
                    .copy_from_user_protected(&mut bounce[..chunk_len], 0)
                    .is_err()
                {
                    return partial_or_fault(bytes_copied);
                }
                match target_vm.access_remote_vm(
                    remote_addr,
                    RemoteAccess::Write(&bounce[..chunk_len]),
                    false,
                ) {
                    Ok(copied) if copied != 0 => copied,
                    Ok(_) | Err(_) => return partial_or_fault(bytes_copied),
                }
            }
        };

        bytes_copied += copied;
        cursor.advance(copied);
        if copied < chunk_len {
            break;
        }
    }

    Ok(bytes_copied)
}

syscall_table_macros::declare_syscall!(SYS_PROCESS_VM_READV, SysProcessVmReadvHandle);
syscall_table_macros::declare_syscall!(SYS_PROCESS_VM_WRITEV, SysProcessVmWritevHandle);
