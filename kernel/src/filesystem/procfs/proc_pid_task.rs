use alloc::{
    string::{String, ToString},
    sync::Arc,
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    filesystem::vfs::{utils::DName, FileType, IndexNode, InodeMode},
    process::{ProcessManager, RawPid},
};

use super::{LockedProcFSInode, ProcFileType};

impl LockedProcFSInode {
    /// 动态列出 `/proc/<pid>/task` 下的 tid 列表。
    ///
    /// 最小实现：仅返回主线程 tid=pid，满足 BusyBox 的 PSSCAN_TASKS 逻辑。
    pub(super) fn dynamical_list_task_tids(&self) -> Result<Vec<String>, SystemError> {
        let pid = self.0.lock().fdata.pid.ok_or(SystemError::EINVAL)?;
        // 尝试确认进程存在；不存在则返回 ESRCH
        let _ = ProcessManager::find_task_by_vpid(pid).ok_or(SystemError::ESRCH)?;
        Ok(vec![pid.to_string()])
    }

    /// 动态创建 `/proc/<pid>/task/<tid>` 目录，并在其中创建 `stat` 文件。
    pub(super) fn dynamical_find_task_tid(
        &self,
        tid: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        let pid = self.0.lock().fdata.pid.ok_or(SystemError::EINVAL)?;
        let tid_u = tid.parse::<usize>().map_err(|_| SystemError::EINVAL)?;
        let tid_pid = RawPid::new(tid_u);

        // 最小实现：只允许 tid==pid（主线程）。
        if tid_pid != pid {
            return Err(SystemError::ENOENT);
        }

        let name = DName::from(tid);

        // Fast-path: 检查目录是否已存在
        {
            let guard = self.0.lock();
            if let Some(existing) = guard.children.get(&name) {
                return Ok(existing.clone());
            }
        }

        // Slow-path: 尝试创建目录，处理并发竞争
        let tid_dir = match self.create(tid, FileType::Dir, InodeMode::from_bits_truncate(0o555)) {
            Ok(dir) => dir,
            Err(SystemError::EEXIST) => {
                // 并发竞争：其他线程已经创建了目录，重新查找并返回
                let guard = self.0.lock();
                if let Some(existing) = guard.children.get(&name) {
                    return Ok(existing.clone());
                } else {
                    // 极不可能的情况：文件系统报告 EEXIST 但内存映射中不存在
                    return Err(SystemError::ENOENT);
                }
            }
            Err(e) => return Err(e),
        };

        let tid_dir_proc = tid_dir
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .ok_or(SystemError::EPERM)?;
        {
            let mut guard = tid_dir_proc.0.lock();
            guard.fdata.pid = Some(pid);
            guard.fdata.tid = Some(tid_pid);
            guard.fdata.ftype = ProcFileType::ProcPidTaskTidDir;
        }

        // 预创建 stat 文件，减少后续 find 分支复杂度
        // 同样需要处理并发竞争
        match tid_dir.create("stat", FileType::File, InodeMode::S_IRUGO) {
            Ok(stat) => {
                // 成功创建 stat 文件，配置其元数据
                let stat_proc = stat
                    .as_any_ref()
                    .downcast_ref::<LockedProcFSInode>()
                    .ok_or(SystemError::EPERM)?;
                {
                    let mut guard = stat_proc.0.lock();
                    guard.fdata.pid = Some(pid);
                    guard.fdata.tid = Some(tid_pid);
                    guard.fdata.ftype = ProcFileType::ProcPidTaskTidStat;
                }
            }
            Err(SystemError::EEXIST) => {
                // 并发竞争：其他线程已经创建了 stat 文件
                // 假设其他线程已经正确配置了 fdata，直接返回 tid_dir
            }
            Err(e) => return Err(e),
        }

        Ok(tid_dir)
    }

    /// 在 `/proc/<pid>/task/<tid>` 目录下动态查找子节点（目前只支持 `stat`）。
    pub(super) fn dynamical_find_task_tid_child(
        &self,
        name: &str,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        if name != "stat" {
            return Err(SystemError::ENOENT);
        }

        let dname = DName::from(name);

        // Fast-path: 若已存在，直接返回（避免递归调用 find）
        {
            let guard = self.0.lock();
            if let Some(existing) = guard.children.get(&dname) {
                return Ok(existing.clone());
            }
        }

        let pid = self.0.lock().fdata.pid.ok_or(SystemError::EINVAL)?;
        let tid = self.0.lock().fdata.tid.ok_or(SystemError::EINVAL)?;

        // Slow-path: 尝试创建文件，处理并发竞争
        let stat = match self.create("stat", FileType::File, InodeMode::S_IRUGO) {
            Ok(file) => file,
            Err(SystemError::EEXIST) => {
                // 并发竞争：其他线程已经创建了文件，重新查找并返回
                let guard = self.0.lock();
                if let Some(existing) = guard.children.get(&dname) {
                    return Ok(existing.clone());
                } else {
                    // 极不可能的情况：文件系统报告 EEXIST 但内存映射中不存在
                    return Err(SystemError::ENOENT);
                }
            }
            Err(e) => return Err(e),
        };

        let stat_proc = stat
            .as_any_ref()
            .downcast_ref::<LockedProcFSInode>()
            .ok_or(SystemError::EPERM)?;
        {
            let mut guard = stat_proc.0.lock();
            guard.fdata.pid = Some(pid);
            guard.fdata.tid = Some(tid);
            guard.fdata.ftype = ProcFileType::ProcPidTaskTidStat;
        }
        Ok(stat)
    }
}
