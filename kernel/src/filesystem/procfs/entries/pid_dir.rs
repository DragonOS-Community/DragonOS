use alloc::{string::{String, ToString}, sync::Arc, vec::Vec};
use system_error::SystemError;
use log::debug;

use crate::{
    libs::{casting::DowncastArc, spinlock::SpinLockGuard},
    filesystem::{
        kernfs::{KernFSInode, callback::KernInodePrivateData},
        vfs::{
            syscall::ModeType, IndexNode, Metadata, FilePrivateData,
            FileType, file::FileMode
        },
        procfs::ProcFS,
    },
    process::ProcessManager,
};

use super::super::file::{ProcFSKernPrivateData, PROCFS_CALLBACK_RO};
use super::super::data::process_info::{ProcProcessInfoType, ProcessId};
use super::dir::ProcDirType;

/// ProcFS PID目录 IndexNode 包装器
/// 重写 list 方法以返回PID目录的标准条目列表
#[derive(Debug)]
pub struct ProcFSPidInode {
    kernfs_inode: Arc<KernFSInode>,
    pid: ProcessId,
}

/// ProcFS Namespace目录 IndexNode 包装器
/// 重写 list 方法以返回Namespace目录的标准条目列表
#[derive(Debug)]
pub struct ProcFSNsInode {
    kernfs_inode: Arc<KernFSInode>,
    pid: ProcessId,
}

impl ProcFSPidInode {
    pub fn new(kernfs_inode: Arc<KernFSInode>, pid: ProcessId) -> Arc<Self> {
        Arc::new(Self {
            kernfs_inode,
            pid,
        })
    }

    /// 动态创建进程信息文件（模仿Linux的proc_pident_lookup）
    fn create_process_info_file(&self, name: &str, info_type: ProcProcessInfoType) -> Result<Arc<dyn IndexNode>, SystemError> {
        debug!("ProcFSPidInode::create_process_info_file: Creating '{}' for PID {}", name, self.pid.data());
        
        let private_data = ProcFSKernPrivateData::ProcessInfo(self.pid, info_type);
        let mode = match name {
            "comm" | "oom_adj" | "oom_score_adj" | "loginuid" => {
                ModeType::S_IFREG | ModeType::from_bits_truncate(0o644) // 可读写
            }
            "environ" => {
                ModeType::S_IFREG | ModeType::from_bits_truncate(0o400) // 只读（受限）
            }
            _ => {
                ModeType::S_IFREG | ModeType::from_bits_truncate(0o444) // 只读
            }
        };

        let file_inode = self.kernfs_inode.create_temporary_file(
            name,
            mode,
            Some(4096),
            Some(KernInodePrivateData::ProcFS(private_data)),
            Some(&PROCFS_CALLBACK_RO),
        )?;

        debug!("ProcFSPidInode::create_process_info_file: Successfully created '{}' for PID {}", name, self.pid.data());
        Ok(file_inode as Arc<dyn IndexNode>)
    }

    /// 动态创建ns目录（模仿Linux的proc_ns_dir_lookup）
    fn create_ns_directory(&self) -> Result<Arc<dyn IndexNode>, SystemError> {
        debug!("ProcFSPidInode::create_ns_directory: Creating ns directory for PID {}", self.pid.data());
        
        let ns_dir = self.kernfs_inode.create_temporary_dir(
            "ns",
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555),
            Some(KernInodePrivateData::ProcFS(
                ProcFSKernPrivateData::Dir(ProcDirType::ProcessNsDir(self.pid))
            )),
        )?;

        // 创建ns目录下的符号链接文件
        self.create_ns_link(&ns_dir, "mnt", ProcProcessInfoType::NsMnt)?;
        self.create_ns_link(&ns_dir, "pid", ProcProcessInfoType::NsPid)?;
        self.create_ns_link(&ns_dir, "cgroup", ProcProcessInfoType::NsCgroup)?;
        self.create_ns_link(&ns_dir, "ipc", ProcProcessInfoType::NsIpc)?;
        self.create_ns_link(&ns_dir, "net", ProcProcessInfoType::NsNet)?;
        self.create_ns_link(&ns_dir, "user", ProcProcessInfoType::NsUser)?;
        self.create_ns_link(&ns_dir, "uts", ProcProcessInfoType::NsUts)?;
        self.create_ns_link(&ns_dir, "time", ProcProcessInfoType::NsTime)?;

        debug!("ProcFSPidInode::create_ns_directory: Successfully created ns directory for PID {}", self.pid.data());
        
        // 返回包装后的ns目录inode，提供正确的list实现
        let wrapped_ns_dir = ProcFSNsInode::new(ns_dir, self.pid);
        Ok(wrapped_ns_dir as Arc<dyn IndexNode>)
    }

    /// 创建namespace符号链接
    fn create_ns_link(&self, parent: &Arc<KernFSInode>, name: &str, info_type: ProcProcessInfoType) -> Result<Arc<KernFSInode>, SystemError> {
        let private_data = ProcFSKernPrivateData::ProcessInfo(self.pid, info_type);

        parent.create_temporary_file(
            name,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777),
            Some(4096),
            Some(KernInodePrivateData::ProcFS(private_data)),
            Some(&PROCFS_CALLBACK_RO),
        )
    }
}

impl ProcFSNsInode {
    pub fn new(kernfs_inode: Arc<KernFSInode>, pid: ProcessId) -> Arc<Self> {
        Arc::new(Self {
            kernfs_inode,
            pid,
        })
    }
}

impl IndexNode for ProcFSPidInode {
    fn list(&self) -> Result<Vec<String>, SystemError> {
        debug!("ProcFSPidInode::list: Listing PID {} directory contents", self.pid.data());
        
        // 返回PID目录的标准条目列表
        let entries = vec![
            "stat".to_string(),
            "status".to_string(),
            "cmdline".to_string(),
            "comm".to_string(),
            "environ".to_string(),
            "maps".to_string(),
            "limits".to_string(),
            "statm".to_string(),
            "oom_score".to_string(),
            "oom_adj".to_string(),
            "cgroup".to_string(),
            "oom_score_adj".to_string(),
            "loginuid".to_string(),
            "sessionid".to_string(),
            "ns".to_string(),
        ];
        
        debug!("ProcFSPidInode::list: Returning {} entries for PID {}", entries.len(), self.pid.data());
        Ok(entries)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        debug!("ProcFSPidInode::find: Looking for '{}' in PID {} directory", name, self.pid.data());
        
        // 模仿Linux内核的proc_pident_lookup机制
        // 动态创建请求的文件inode，而不是预先创建
        match name {
            "stat" => self.create_process_info_file(name, ProcProcessInfoType::Stat),
            "status" => self.create_process_info_file(name, ProcProcessInfoType::Status),
            "cmdline" => self.create_process_info_file(name, ProcProcessInfoType::CmdLine),
            "comm" => self.create_process_info_file(name, ProcProcessInfoType::Comm),
            "environ" => self.create_process_info_file(name, ProcProcessInfoType::Environ),
            "maps" => self.create_process_info_file(name, ProcProcessInfoType::Maps),
            "limits" => self.create_process_info_file(name, ProcProcessInfoType::Limits),
            "statm" => self.create_process_info_file(name, ProcProcessInfoType::StatM),
            "oom_score" => self.create_process_info_file(name, ProcProcessInfoType::OomScore),
            "oom_adj" => self.create_process_info_file(name, ProcProcessInfoType::OomAdj),
            "cgroup" => self.create_process_info_file(name, ProcProcessInfoType::Cgroup),
            "oom_score_adj" => self.create_process_info_file(name, ProcProcessInfoType::OomScoreAdj),
            "loginuid" => self.create_process_info_file(name, ProcProcessInfoType::Loginuid),
            "sessionid" => self.create_process_info_file(name, ProcProcessInfoType::Sessionid),
            "ns" => self.create_ns_directory(),
            _ => {
                debug!("ProcFSPidInode::find: '{}' is not a valid PID file", name);
                Err(SystemError::ENOENT)
            }
        }
    }

    // 委托其他方法到 kernfs_inode
    fn open(
        &self,
        data: SpinLockGuard<FilePrivateData>,
        mode: &FileMode,
    ) -> Result<(), SystemError> {
        self.kernfs_inode.open(data, mode)
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.kernfs_inode.close(data)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.read_at(offset, len, buf, data)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.write_at(offset, len, buf, data)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.kernfs_inode.metadata()
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.kernfs_inode.set_metadata(metadata)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.kernfs_inode.resize(len)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.kernfs_inode.create_with_data(name, file_type, mode, data)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.kernfs_inode.link(name, other)
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.kernfs_inode.unlink(name)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.kernfs_inode.rmdir(name)
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        self.kernfs_inode.move_to(old_name, target, new_name)
    }

    fn get_entry_name(&self, ino: crate::filesystem::vfs::InodeId) -> Result<String, SystemError> {
        self.kernfs_inode.get_entry_name(ino)
    }

    fn get_entry_name_and_metadata(
        &self,
        ino: crate::filesystem::vfs::InodeId,
    ) -> Result<(String, Metadata), SystemError> {
        self.kernfs_inode.get_entry_name_and_metadata(ino)
    }

    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.ioctl(cmd, arg, private_data)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.kernfs_inode.fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

impl IndexNode for ProcFSNsInode {
    fn list(&self) -> Result<Vec<String>, SystemError> {
        debug!("ProcFSNsInode::list: Listing namespace directory contents for PID {}", self.pid.data());
        
        // 返回Namespace目录的标准条目列表
        let entries = vec![
            "mnt".to_string(),
            "pid".to_string(),
            "cgroup".to_string(),
            "ipc".to_string(),
            "net".to_string(),
            "user".to_string(),
            "uts".to_string(),
            "time".to_string(),
        ];
        
        debug!("ProcFSNsInode::list: Returning {} entries for namespace directory of PID {}", entries.len(), self.pid.data());
        Ok(entries)
    }

    fn find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        debug!("ProcFSNsInode::find: Looking for '{}' in namespace directory for PID {}", name, self.pid.data());
        
        // 创建请求的namespace符号链接文件
        let info_type = match name {
            "mnt" => ProcProcessInfoType::NsMnt,
            "pid" => ProcProcessInfoType::NsPid,
            "cgroup" => ProcProcessInfoType::NsCgroup,
            "ipc" => ProcProcessInfoType::NsIpc,
            "net" => ProcProcessInfoType::NsNet,
            "user" => ProcProcessInfoType::NsUser,
            "uts" => ProcProcessInfoType::NsUts,
            "time" => ProcProcessInfoType::NsTime,
            _ => {
                debug!("ProcFSNsInode::find: '{}' is not a valid namespace file", name);
                return Err(SystemError::ENOENT);
            }
        };

        let private_data = ProcFSKernPrivateData::ProcessInfo(self.pid, info_type);
        let file_inode = self.kernfs_inode.create_temporary_file(
            name,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777),
            Some(4096),
            Some(KernInodePrivateData::ProcFS(private_data)),
            Some(&PROCFS_CALLBACK_RO),
        )?;

        debug!("ProcFSNsInode::find: Successfully created namespace link '{}' for PID {}", name, self.pid.data());
        Ok(file_inode as Arc<dyn IndexNode>)
    }

    // 委托其他方法到 kernfs_inode
    fn open(
        &self,
        data: SpinLockGuard<FilePrivateData>,
        mode: &FileMode,
    ) -> Result<(), SystemError> {
        self.kernfs_inode.open(data, mode)
    }

    fn close(&self, data: SpinLockGuard<FilePrivateData>) -> Result<(), SystemError> {
        self.kernfs_inode.close(data)
    }

    fn read_at(
        &self,
        offset: usize,
        len: usize,
        buf: &mut [u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.read_at(offset, len, buf, data)
    }

    fn write_at(
        &self,
        offset: usize,
        len: usize,
        buf: &[u8],
        data: SpinLockGuard<FilePrivateData>,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.write_at(offset, len, buf, data)
    }

    fn metadata(&self) -> Result<Metadata, SystemError> {
        self.kernfs_inode.metadata()
    }

    fn set_metadata(&self, metadata: &Metadata) -> Result<(), SystemError> {
        self.kernfs_inode.set_metadata(metadata)
    }

    fn resize(&self, len: usize) -> Result<(), SystemError> {
        self.kernfs_inode.resize(len)
    }

    fn create_with_data(
        &self,
        name: &str,
        file_type: FileType,
        mode: ModeType,
        data: usize,
    ) -> Result<Arc<dyn IndexNode>, SystemError> {
        self.kernfs_inode.create_with_data(name, file_type, mode, data)
    }

    fn link(&self, name: &str, other: &Arc<dyn IndexNode>) -> Result<(), SystemError> {
        self.kernfs_inode.link(name, other)
    }

    fn unlink(&self, name: &str) -> Result<(), SystemError> {
        self.kernfs_inode.unlink(name)
    }

    fn rmdir(&self, name: &str) -> Result<(), SystemError> {
        self.kernfs_inode.rmdir(name)
    }

    fn move_to(
        &self,
        old_name: &str,
        target: &Arc<dyn IndexNode>,
        new_name: &str,
    ) -> Result<(), SystemError> {
        self.kernfs_inode.move_to(old_name, target, new_name)
    }

    fn get_entry_name(&self, ino: crate::filesystem::vfs::InodeId) -> Result<String, SystemError> {
        self.kernfs_inode.get_entry_name(ino)
    }

    fn get_entry_name_and_metadata(
        &self,
        ino: crate::filesystem::vfs::InodeId,
    ) -> Result<(String, Metadata), SystemError> {
        self.kernfs_inode.get_entry_name_and_metadata(ino)
    }

    fn ioctl(
        &self,
        cmd: u32,
        arg: usize,
        private_data: &FilePrivateData,
    ) -> Result<usize, SystemError> {
        self.kernfs_inode.ioctl(cmd, arg, private_data)
    }

    fn fs(&self) -> Arc<dyn crate::filesystem::vfs::FileSystem> {
        self.kernfs_inode.fs()
    }

    fn as_any_ref(&self) -> &dyn core::any::Any {
        self
    }
}

impl ProcFS {

    #[allow(dead_code)] // 查找或创建 /proc/<pid> 目录（按名称）
    fn find_process_directory(&self, name: &str) -> Result<Arc<KernFSInode>, SystemError> {
        // name 是命名空间内的 PID 字符串
        let _ = name.parse::<usize>().map_err(|_| SystemError::ENOENT)?;

        // 如果目录已存在，直接返回
        if let Ok(existing_dir) = self.root_inode().find(name) {
            return Ok(existing_dir.downcast_arc::<KernFSInode>().ok_or(SystemError::EINVAL)?);
        }

        // 无法从字符串安全构造 RawPid（构造器私有），这里不尝试反查 PCB。
        // 由上层在拿到真实 ProcessId 后调用 create_single_process_directory。
        Err(SystemError::ENOENT)
    }


    /// 创建临时的进程目录，不挂载到父目录的children中
    /// 这是纯动态模式的核心方法，所有PID目录都通过此方法临时创建
    /// 返回包装后的PID目录inode，提供正确的list实现
    pub fn create_temporary_process_directory(&self, pid: ProcessId, ns_pid_name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        // 1) 确认进程存在
        let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;

        // 2) 获取该进程在当前挂载实例的 pidns 中的 PID 号
        let ns = &self.pid_namespace();
        let process_pid = pcb.pid();
        let ns_pid = process_pid.pid_nr_ns(ns);
        
        // 如果该进程在当前命名空间中不可见，返回 ENOENT
        if ns_pid.data() == 0 {
            ::log::warn!("create_temporary_process_directory: Process {} not visible in namespace (level {})", 
                         pid.data(), ns.level());
            return Err(SystemError::ENOENT);
        }
        
        // ::log::debug!("create_temporary_process_directory: Process {} mapped to ns_pid {} in namespace level {}", 
        //               pid.data(), ns_pid.data(), ns.level());

        // 3) 创建临时进程目录，使用 KernFS 的临时节点创建功能
        use crate::filesystem::kernfs::callback::KernInodePrivateData;
        use crate::filesystem::vfs::syscall::ModeType;
        
        let process_dir = self.root_inode().create_temporary_dir(
            ns_pid_name,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555),
            Some(KernInodePrivateData::ProcFS(
                super::super::file::ProcFSKernPrivateData::Dir(
                    super::dir::ProcDirType::ProcessDir(pid)
                )
            )),
        )?;

        // 4) 不再预先创建文件，模仿Linux内核的做法
        // 文件将在ProcFSPidInode::find中按需动态创建

        // ::log::debug!("create_temporary_process_directory: Successfully created empty PID directory for PID {} (ns_pid {}) - files will be created on demand", 
        //               pid.data(), ns_pid_name);
        
        // 5) 返回包装后的PID目录inode，提供Linux风格的动态文件创建
        let wrapped_pid_dir = ProcFSPidInode::new(process_dir, pid);
        Ok(wrapped_pid_dir as Arc<dyn IndexNode>)
    }

    /// 在纯动态模式下的进程目录清理
    /// 不再依赖PCB存在来清理目录，因为目录本身就是临时创建的
    pub fn remove_process_directory(&self, pid: ProcessId) -> Result<(), SystemError> {
        ::log::info!(
            "remove_process_directory: Process {} exited in namespace level {} (pure dynamic mode - no active cleanup needed)",
            pid.data(),
            self.pid_namespace().level()
        );
        
        // 在纯动态模式下，进程目录是临时创建的，不需要主动清理
        // 当进程不存在时，dynamic_find 和 dynamic_list 会自动排除该进程
        // 临时节点的生命周期由Arc引用计数管理
        
        // 记录日志以便调试
        if !ProcessManager::initialized() {
            ::log::warn!(
                "remove_process_directory: ProcessManager not initialized for PID {}",
                pid.data()
            );
            return Ok(());
        }
        
        // 验证进程确实已退出（仅用于日志记录）
        if let Some(pcb) = ProcessManager::find(pid) {
            let state = pcb.sched_info().inner_lock_read_irqsave().state();
            if state.is_exited() {
                ::log::info!(
                    "remove_process_directory: Confirmed PID {} has exited state",
                    pid.data()
                );
            } else {
                ::log::warn!(
                    "remove_process_directory: PID {} called for exit but still in running state",
                    pid.data()
                );
            }
        } else {
            ::log::info!(
                "remove_process_directory: PID {} no longer found in ProcessManager",
                pid.data()
            );
        }
        
        ::log::info!(
            "remove_process_directory: COMPLETED for PID {} (pure dynamic mode)",
            pid.data()
        );
        Ok(())
    }

}