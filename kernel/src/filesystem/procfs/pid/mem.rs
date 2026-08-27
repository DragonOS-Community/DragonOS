//! /proc/[pid]/mem - access to the target process's virtual address space.
//!
//! The file offset is a user virtual address of the target process; read/write directly
//! access that address space. Reuses ptrace's batched cross-process memory access
//! (page-table translation + fault-in on page faults).

use crate::{
    filesystem::{
        procfs::{
            pid::ProcPidTarget,
            template::{Builder, FileOps, ProcFileBuilder},
            ProcfsFilePrivateData,
        },
        vfs::{FilePrivateData, IndexNode, InodeMode},
    },
    libs::mutex::MutexGuard,
    mm::{remote_access::RemoteAccess, ucontext::AddressSpace},
    process::{ptrace::PtraceAccessCreds, ProcessControlBlock},
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// FileOps implementation for /proc/[pid]/mem.
#[derive(Debug)]
pub struct MemFileOps {
    target: ProcPidTarget,
}

impl MemFileOps {
    pub fn new_inode(target: ProcPidTarget, parent: Weak<dyn IndexNode>) -> Arc<dyn IndexNode> {
        ProcFileBuilder::new(Self { target }, InodeMode::S_IRUSR | InodeMode::S_IWUSR)
            .parent(parent)
            .build()
            .unwrap()
    }

    /// Permission check (called only once at open time).
    fn check_access(target: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let current = crate::process::ProcessManager::current_pcb();
        if current.has_permission_to_trace(target, PtraceAccessCreds::FsCreds) {
            Ok(())
        } else {
            Err(SystemError::EACCES)
        }
    }

    /// Extract the address space pinned at open time from the file private data.
    fn pinned_vm_from_data(data: &MutexGuard<FilePrivateData>) -> Option<Arc<AddressSpace>> {
        let FilePrivateData::Procfs(ProcfsFilePrivateData { pinned_vm, .. }) = &**data else {
            return None;
        };
        pinned_vm.clone()
    }
}

impl FileOps for MemFileOps {
    fn open(&self, data: &mut MutexGuard<FilePrivateData>) -> Result<(), SystemError> {
        let target = self.target.task().ok_or(SystemError::ESRCH)?;
        let pinned = {
            let _exec_guard = target.exec_update_read();
            Self::check_access(&target)?;
            target.basic().user_vm().ok_or(SystemError::ESRCH)?
        };
        let mut new_data = ProcfsFilePrivateData::new();
        new_data.pinned_vm = Some(pinned);
        **data = FilePrivateData::Procfs(new_data);
        Ok(())
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        // Only touch the address space pinned at open time.
        let Some(pinned_vm) = Self::pinned_vm_from_data(&data) else {
            return Ok(0);
        };
        let Some(_mm_guard) = pinned_vm.try_acquire() else {
            return Ok(0);
        };
        let actual_len = len.min(buf.len());
        // Batched cross-process read
        let n =
            pinned_vm.access_remote_vm(offset, RemoteAccess::Read(&mut buf[..actual_len]), true)?;
        // A zero read may race with tear-down; re-check once and treat as EOF if done.
        if n == 0 && actual_len > 0 && pinned_vm.is_torn_down() {
            return Ok(0);
        }
        // The first byte is inaccessible (including offset beyond USER_END): return EIO like mem_rw
        if n == 0 && actual_len > 0 {
            return Err(SystemError::EIO);
        }
        Ok(n)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: MutexGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        if len == 0 {
            return Ok(0);
        }
        let Some(pinned_vm) = Self::pinned_vm_from_data(&data) else {
            return Ok(0);
        };
        // After tear-down, writes are also treated as EOF (0)
        let Some(_mm_guard) = pinned_vm.try_acquire() else {
            return Ok(0);
        };
        let actual_len = len.min(buf.len());
        let n =
            pinned_vm.access_remote_vm(offset, RemoteAccess::Write(&buf[..actual_len]), true)?;
        if n == 0 && actual_len > 0 && pinned_vm.is_torn_down() {
            return Ok(0);
        }
        // The first byte is inaccessible: return EIO like mem_rw.
        if n == 0 && actual_len > 0 {
            return Err(SystemError::EIO);
        }
        Ok(n)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.target.owner_uid_gid()
    }
}
