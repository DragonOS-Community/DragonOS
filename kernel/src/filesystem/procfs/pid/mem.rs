//! /proc/[pid]/mem - 目标进程虚拟地址空间访问。
//!
//! 文件偏移量即目标进程的用户虚拟地址，read/write 直接读写该地址空间。
//! 复用 ptrace 的批量跨进程内存访问（页表翻译 + 缺页 fault-in）。

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
    mm::ucontext::AddressSpace,
    process::ProcessControlBlock,
};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;

/// /proc/[pid]/mem 的 FileOps 实现。
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

    /// 权限检查（仅在 open 时调用一次）。
    fn check_access(target: &Arc<ProcessControlBlock>) -> Result<(), SystemError> {
        let current = crate::process::ProcessManager::current_pcb();
        if current.has_permission_to_trace(target) {
            Ok(())
        } else {
            Err(SystemError::EACCES)
        }
    }

    /// 从文件私有数据取出打开时钉住的目标地址空间。
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
        // 只访问打开时钉住的地址空间。
        let Some(pinned_vm) = Self::pinned_vm_from_data(&data) else {
            return Ok(0);
        };
        if pinned_vm.is_torn_down() {
            return Ok(0);
        }
        let actual_len = len.min(buf.len());
        // 批量跨进程读：单次 AddressSpace 读锁内按页整段拷贝，避免逐字节取锁+走查页表。
        let n = ProcessControlBlock::access_user_chunk_on_vm_read(
            &pinned_vm,
            offset,
            &mut buf[..actual_len],
        )?;
        // 读到 0 字节可能是“恰在拆除临界区边界”的竞态。复判一次，拆除已成事实则按 EOF 处理
        if n == 0 && actual_len > 0 && pinned_vm.is_torn_down() {
            return Ok(0);
        }
        // 首字节即不可访问（含 offset 越过 USER_END）：返回 EIO，mem_rw
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
        // 拆除后写同样按 EOF（0）处理
        if pinned_vm.is_torn_down() {
            return Ok(0);
        }
        let actual_len = len.min(buf.len());
        let n = ProcessControlBlock::access_user_chunk_on_vm_write(
            &pinned_vm,
            offset,
            &buf[..actual_len],
        )?;
        if n == 0 && actual_len > 0 && pinned_vm.is_torn_down() {
            return Ok(0);
        }
        // 首字节即不可访问：返回 EIO，mem_rw。
        if n == 0 && actual_len > 0 {
            return Err(SystemError::EIO);
        }
        Ok(n)
    }

    fn owner(&self) -> Option<(usize, usize)> {
        self.target.owner_uid_gid()
    }
}
