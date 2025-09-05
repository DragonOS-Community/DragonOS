use core::fmt::Debug;

use super::{
    kernfs::{
        callback::KernInodePrivateData,
        KernFS, KernFSInode,
    },
    vfs::{syscall::ModeType, FileSystem, IndexNode, mount::MountFlags},
};

use crate::{
    libs::{casting::DowncastArc, once::Once},
    process::ProcessManager,
};
use alloc::{string::{String, ToString}, sync::Arc, vec::Vec};
use ::log::{info, warn};
use system_error::SystemError;

pub mod system_info;
pub mod process_info;
pub mod sys_config;
pub mod dir;
pub mod file;

pub use system_info::{ProcSystemInfoType, get_system_info};
pub use process_info::{ProcProcessInfoType, ProcessId, get_process_info};
pub use sys_config::{ProcSysConfigType, get_sys_config, set_sys_config, ConfigValue, init_system_configs};
pub use dir::ProcDirType;
use file::{PROCFS_CALLBACK_RO, PROCFS_CALLBACK_RW, PROCFS_CALLBACK_WO};


pub static mut PROCFS_INSTANCE: Option<ProcFS> = None;

#[inline(always)]
pub fn procfs_instance() -> &'static ProcFS {
    unsafe {
        return PROCFS_INSTANCE.as_ref().unwrap();
    }
}

pub fn procfs_init() -> Result<(), SystemError> {
    static INIT: Once = Once::new();
    let mut result = None;
    INIT.call_once(|| {
        info!("Initializing ProcFS...");

        let procfs = ProcFS::new();
        unsafe { PROCFS_INSTANCE = Some(procfs) };
        let root_inode = ProcessManager::current_mntns().root_inode();
        
        root_inode
            .mkdir("proc", ModeType::from_bits_truncate(0o755))
            .expect("Unable to create /proc")
            .mount(procfs_instance().fs().clone(), MountFlags::empty())
            .expect("Failed to mount at /proc");
        
        match process_info::register_all_current_processes() {
            Ok(_) => info!("Successfully registered all current processes"),
            Err(e) => warn!("Failed to register some processes: {:?}", e),
        }
        
        info!("ProcFS mounted and initialized");
        result = Some(Ok(()));
    });

    return result.unwrap();
}

pub fn procfs_register_pid(pid: ProcessId) -> Result<(), SystemError> {
    if let Some(procfs) = unsafe { PROCFS_INSTANCE.as_ref() } {
        procfs.create_single_process_directory(pid)?;
    }
    Ok(())
}

pub fn procfs_unregister_pid(pid: ProcessId) -> Result<(), SystemError> {
    if let Some(procfs) = unsafe { PROCFS_INSTANCE.as_ref() } {
        procfs.remove_process_directory(pid)?
    }
    Ok(())
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum ProcFSKernPrivateData {
    /// 系统信息文件
    SystemInfo(ProcSystemInfoType),
    /// 进程信息文件 
    ProcessInfo(ProcessId, ProcProcessInfoType),
    /// 系统配置文件
    SysConfig(ProcSysConfigType),
    /// 目录类型
    Dir(ProcDirType),
    /// 进程列表
    ProcessList(Vec<ProcessId>),
}

impl ProcFSKernPrivateData {
    /// 统一的读取接口
    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        use ::log::info;
        // info!("ProcFSKernPrivateData::callback_read called, type: {:?}", self);
        
        let content = match self {
            ProcFSKernPrivateData::SystemInfo(info_type) => {
                // info!("Getting system info for type: {:?}", info_type);
                get_system_info(*info_type)?
            }
            ProcFSKernPrivateData::ProcessInfo(pid, info_type) => {
                // info!("Getting process info for PID: {}, type: {:?}", pid.data(), info_type);
                
                if ProcessManager::find(*pid).is_none() {
                    return Err(SystemError::ESRCH);
                }
                
                get_process_info(*pid, *info_type)?
            }
            ProcFSKernPrivateData::SysConfig(config_type) => {
                get_sys_config(*config_type)?
            }
            ProcFSKernPrivateData::Dir(dir_type) => {
                match dir_type {
                    ProcDirType::ProcessDir(pid) => {
                        if ProcessManager::find(*pid).is_none() {
                            return Err(SystemError::ENOENT);
                        }
                        return Err(SystemError::EISDIR);
                    }
                    _ => return Err(SystemError::EISDIR),
                }
            }
            ProcFSKernPrivateData::ProcessList(process_list) => {
                let mut content = String::new();
                for pid in process_list {
                    if ProcessManager::find(*pid).is_some() {
                        content.push_str(&format!("{}\n", pid.data()));
                    }
                }
                content
            }
        };

        // info!("Generated content length: {}", content.len());
        
        let content_bytes = content.as_bytes();
        if offset >= content_bytes.len() {
            return Ok(0);
        }

        let len = core::cmp::min(buf.len(), content_bytes.len() - offset);
        buf[..len].copy_from_slice(&content_bytes[offset..offset + len]);
        // info!("Returning {} bytes", len);
        Ok(len)
    }

    /// 统一的写入接口
    pub fn callback_write(&self, buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        match self {
            ProcFSKernPrivateData::SysConfig(config_type) => {
                set_sys_config(*config_type, buf)
            }
            _ => Err(SystemError::EPERM), 
        }
    }
}

#[derive(Debug)]
pub struct ProcFS {
    /// 根inode
    root_inode: Arc<KernFSInode>,
    /// kernfs实例
    kernfs: Arc<KernFS>,
}

impl ProcFS {
    pub fn new() -> Self {
        let kernfs: Arc<KernFS> = KernFS::new("proc");
        let root_inode: Arc<KernFSInode> = kernfs.root_inode().downcast_arc().unwrap();
        
        let procfs = ProcFS { root_inode, kernfs };
        
        init_system_configs();
        
        procfs.init_directory_structure().expect("Failed to initialize ProcFS");
        
        procfs
    }

    pub fn root_inode(&self) -> &Arc<KernFSInode> {
        &self.root_inode
    }

    pub fn fs(&self) -> &Arc<KernFS> {
        &self.kernfs
    }
    
    pub fn get_current_process_list(&self) -> Result<Vec<ProcessId>, SystemError> {
        if !ProcessManager::initialized() {
            // info!("Process management system not initialized yet, returning empty process list");
            return Ok(Vec::new());
        }
        
        let all_pids = ProcessManager::get_all_processes();
        
        let mut process_list = Vec::new();
        for pid in all_pids {
            if ProcessManager::find(pid).is_some() {
                process_list.push(pid);
            }
        }
        
        // info!("Found {} active processes", process_list.len());
        Ok(process_list)
    }

    pub fn readdir_root(&self, offset: usize) -> Result<Vec<String>, SystemError> {
        let mut entries = Vec::new();
        
        let static_entries = vec![
            ".", "..", "version", "cpuinfo", "meminfo", "uptime", 
            "loadavg", "stat", "interrupts", "devices", "filesystems", 
            "mounts", "cmdline", "sys"
        ];
        
        for (i, entry) in static_entries.iter().enumerate() {
            if i >= offset {
                entries.push(entry.to_string());
            }
        }
        
        let current_processes = self.get_current_process_list()?;
        let process_offset = if offset > static_entries.len() { 
            offset - static_entries.len() 
        } else { 
            0 
        };
        
        for (i, pid) in current_processes.iter().enumerate() {
            if i >= process_offset {
                entries.push(pid.to_string());
            }
        }
        
        Ok(entries)
    }

    pub fn custom_find(&self, name: &str) -> Result<Arc<dyn IndexNode>, SystemError> {
        match self.root_inode.find(name) {
            Ok(inode) => return Ok(inode),
            Err(SystemError::ENOENT) => {
                if let Ok(pid_num) = name.parse::<u32>() {
                    let pid = ProcessId::new(pid_num.try_into().map_err(|_| SystemError::EINVAL)?);
                    if ProcessManager::find(pid).is_some() {
                        match self.create_single_process_directory(pid) {
                            Ok(process_dir) => {
                                info!("Dynamically created /proc/{} directory", pid.data());
                                return Ok(process_dir);
                            }
                            Err(e) => {
                                warn!("Failed to create process directory for PID {}: {:?}", pid.data(), e);
                                return Err(e);
                            }
                        }
                    }
                }
                return Err(SystemError::ENOENT);
            }
            Err(e) => return Err(e),
        }
    }

 
    fn init_directory_structure(&self) -> Result<(), SystemError> {
        let root = &self.root_inode;
        
        self.create_system_files(root)?;
        
        self.init_dynamic_process_discovery(root)?;
        
        self.create_sys_directory(root)?;
        
        Ok(())
    }
    
    fn init_dynamic_process_discovery(&self, _root: &Arc<KernFSInode>) -> Result<(), SystemError> {
        // info!("初始化动态进程发现机制");
        Ok(())
    }
    
    pub fn find_process_directory(&self, name: &str) -> Result<Arc<KernFSInode>, SystemError> {
        let pid = name.parse::<u32>().map_err(|_| SystemError::ENOENT)?;
        let process_id = ProcessId::new(pid.try_into().unwrap());
        
        let _pcb = ProcessManager::find(process_id).ok_or(SystemError::ENOENT)?;
        
        if let Ok(existing_dir) = self.root_inode.find(name) {
            return Ok(existing_dir.downcast_arc::<KernFSInode>().ok_or(SystemError::EINVAL)?);
        }
        
        // info!("动态创建进程目录: /proc/{}", pid);
        self.create_single_process_directory(process_id)
    }

    fn create_system_files(&self, root: &Arc<KernFSInode>) -> Result<(), SystemError> {
        self.create_system_info_file(root, "version", ProcSystemInfoType::Version, 
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "cpuinfo", ProcSystemInfoType::CpuInfo,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "meminfo", ProcSystemInfoType::MemInfo,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "uptime", ProcSystemInfoType::Uptime,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "loadavg", ProcSystemInfoType::LoadAvg,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "stat", ProcSystemInfoType::Stat,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "interrupts", ProcSystemInfoType::Interrupts,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "devices", ProcSystemInfoType::Devices,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "filesystems", ProcSystemInfoType::FileSystems,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "mounts", ProcSystemInfoType::Mounts,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_system_info_file(root, "cmdline", ProcSystemInfoType::CmdLine,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        Ok(())
    }

    pub fn create_system_info_file(
        &self,
        parent: &Arc<KernFSInode>,
        name: &str,
        info_type: ProcSystemInfoType,
        mode: ModeType,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let private_data = ProcFSKernPrivateData::SystemInfo(info_type);
        
        parent.add_file(
            name.to_string(),
            mode,
            Some(4096),
            Some(KernInodePrivateData::ProcFS(private_data)),
            Some(&PROCFS_CALLBACK_RO),
        )
    }

    pub fn create_dir(
        &self,
        parent: &Arc<KernFSInode>,
        name: &str,
        dir_type: ProcDirType,
        mode: ModeType,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let private_data = ProcFSKernPrivateData::Dir(dir_type);
        
        parent.add_dir(
            name.to_string(),
            mode,
            Some(KernInodePrivateData::ProcFS(private_data)),
            None,
        )
    }

    pub fn create_single_process_directory(&self, pid: ProcessId) -> Result<Arc<KernFSInode>, SystemError> {
        let _process = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
        
        let pid_str = pid.to_string();
        let process_dir = self.create_dir(&self.root_inode, &pid_str, 
            ProcDirType::ProcessDir(pid),
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;

        self.create_process_info_file(&process_dir, "stat", pid, ProcProcessInfoType::Stat,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "status", pid, ProcProcessInfoType::Status,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "cmdline", pid, ProcProcessInfoType::CmdLine,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "comm", pid, ProcProcessInfoType::Comm,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?;
        
        self.create_process_info_file(&process_dir, "environ", pid, ProcProcessInfoType::Environ,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o400))?;
        
        self.create_process_info_file(&process_dir, "maps", pid, ProcProcessInfoType::Maps,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "limits", pid, ProcProcessInfoType::Limits,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "statm", pid, ProcProcessInfoType::StatM,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "oom_score", pid, ProcProcessInfoType::OomScore,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "oom_adj", pid, ProcProcessInfoType::OomAdj,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?;

        self.create_process_info_file(&process_dir, "cgroup", pid, ProcProcessInfoType::Cgroup,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;
        
        self.create_process_info_file(&process_dir, "oom_score_adj", pid, ProcProcessInfoType::OomScoreAdj,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?;
        
        self.create_process_info_file(&process_dir, "loginuid", pid, ProcProcessInfoType::Loginuid,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?;
        
        self.create_process_info_file(&process_dir, "sessionid", pid, ProcProcessInfoType::Sessionid,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?;

        self.create_process_ns_directory(&process_dir, pid)?;
        
        // 创建文件描述符目录（容器文件系统隔离）
        // TODO: 需要完善文件描述符管理后启用
        // self.create_process_fd_directory(&process_dir, pid)?;
        
        // info!("Created /proc/{} directory with all process files for verified process", pid);
        Ok(process_dir)
    }

    pub fn create_process_info_file(
        &self,
        parent: &Arc<KernFSInode>,
        name: &str,
        pid: ProcessId,
        info_type: ProcProcessInfoType,
        mode: ModeType,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let private_data = ProcFSKernPrivateData::ProcessInfo(pid, info_type);
        
        parent.add_file(
            name.to_string(),
            mode,
            Some(4096),
            Some(KernInodePrivateData::ProcFS(private_data)),
            Some(&PROCFS_CALLBACK_RO),
        )
    }

    pub fn create_process_ns_directory(&self, process_dir: &Arc<KernFSInode>, pid: ProcessId) -> Result<Arc<KernFSInode>, SystemError> {
        let ns_dir = self.create_dir(process_dir, "ns", ProcDirType::ProcessNsDir(pid),
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_process_info_file(&ns_dir, "mnt", pid, ProcProcessInfoType::NsMnt,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        self.create_process_info_file(&ns_dir, "pid", pid, ProcProcessInfoType::NsPid,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        // 预留其他namespace类型的符号链接
        // TODO: 等DragonOS namespace子系统完善后启用
        // 当前返回占位符数据
        self.create_process_info_file(&ns_dir, "cgroup", pid, ProcProcessInfoType::NsCgroup,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        self.create_process_info_file(&ns_dir, "ipc", pid, ProcProcessInfoType::NsIpc,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        self.create_process_info_file(&ns_dir, "net", pid, ProcProcessInfoType::NsNet,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        self.create_process_info_file(&ns_dir, "user", pid, ProcProcessInfoType::NsUser,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        self.create_process_info_file(&ns_dir, "uts", pid, ProcProcessInfoType::NsUts,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        // 预留接口
        self.create_process_info_file(&ns_dir, "time", pid, ProcProcessInfoType::NsTime,
            ModeType::S_IFLNK | ModeType::from_bits_truncate(0o777))?;
        
        // info!("Created /proc/{}/ns directory with namespace symlinks", pid);
        Ok(ns_dir)
    }

    pub fn remove_process_directory(&self, pid: ProcessId) -> Result<(), SystemError> {
        let pid_str = pid.to_string();
        if let Ok(process_dir) = self.root_inode.find(&pid_str) {
            if let Some(kernfs_inode) = process_dir.downcast_arc::<KernFSInode>() {
                kernfs_inode.remove_inode_include_self();
                // info!("Successfully removed /proc/{} directory and all its contents", pid);
            } else {
                warn!("Failed to downcast process_dir to KernFSInode for PID {}", pid);
                return Err(SystemError::EINVAL);
            }
        }
        Ok(())
    }

    fn create_sys_directory(&self, root: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let sys_dir = self.create_dir(root, "sys", ProcDirType::SysDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;

        self.create_sys_kernel_directory(&sys_dir)?;
      
        self.create_sys_vm_directory(&sys_dir)?;
   
        self.create_sys_fs_directory(&sys_dir)?;
        
        self.create_sys_net_directory(&sys_dir)?;
        
        // info!("Created /proc/sys directory structure");
        Ok(())
    }

    fn create_sys_kernel_directory(&self, sys_dir: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let kernel_dir = self.create_dir(sys_dir, "kernel", ProcDirType::SysKernelDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;

        self.create_sys_config_file(&kernel_dir, "version", ProcSysConfigType::KernelVersion,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?; // 只读
        
        self.create_sys_config_file(&kernel_dir, "hostname", ProcSysConfigType::KernelHostname,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&kernel_dir, "domainname", ProcSysConfigType::KernelDomainname,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&kernel_dir, "ostype", ProcSysConfigType::KernelOstype,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?; // 只读
        
        self.create_sys_config_file(&kernel_dir, "osrelease", ProcSysConfigType::KernelOsrelease,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?; // 只读
        
        self.create_sys_config_file(&kernel_dir, "panic", ProcSysConfigType::KernelPanic,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&kernel_dir, "panic_on_oops", ProcSysConfigType::KernelPanicOnOops,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&kernel_dir, "pid_max", ProcSysConfigType::KernelPidMax,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&kernel_dir, "threads-max", ProcSysConfigType::KernelThreadsMax,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写

        let random_dir = self.create_dir(&kernel_dir, "random", ProcDirType::SysKernelDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_sys_config_file(&random_dir, "boot_id", ProcSysConfigType::KernelRandomBootId,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?; // 只读
        
        // info!("Created /proc/sys/kernel directory");
        Ok(())
    }

    fn create_sys_vm_directory(&self, sys_dir: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let vm_dir = self.create_dir(sys_dir, "vm", ProcDirType::SysVmDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_sys_config_file(&vm_dir, "swappiness", ProcSysConfigType::VmSwappiness,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&vm_dir, "dirty_ratio", ProcSysConfigType::VmDirtyRatio,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&vm_dir, "dirty_background_ratio", ProcSysConfigType::VmDirtyBackgroundRatio,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&vm_dir, "drop_caches", ProcSysConfigType::VmDropCaches,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o200))?; // 只写
        
        self.create_sys_config_file(&vm_dir, "overcommit_memory", ProcSysConfigType::VmOvercommitMemory,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&vm_dir, "overcommit_ratio", ProcSysConfigType::VmOvercommitRatio,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&vm_dir, "min_free_kbytes", ProcSysConfigType::VmMinFreeKbytes,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        // info!("Created /proc/sys/vm directory");
        Ok(())
    }

    fn create_sys_fs_directory(&self, sys_dir: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let fs_dir = self.create_dir(sys_dir, "fs", ProcDirType::SysFsDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_sys_config_file(&fs_dir, "file-max", ProcSysConfigType::FsFileMax,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&fs_dir, "file-nr", ProcSysConfigType::FsFileNr,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?; // 只读
        
        self.create_sys_config_file(&fs_dir, "inode-max", ProcSysConfigType::FsInodeMax,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&fs_dir, "inode-nr", ProcSysConfigType::FsInodeNr,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o444))?; // 只读
        
        self.create_sys_config_file(&fs_dir, "aio-max-nr", ProcSysConfigType::FsAioMaxNr,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        // info!("Created /proc/sys/fs directory");
        Ok(())
    }

    fn create_sys_net_directory(&self, sys_dir: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let net_dir = self.create_dir(sys_dir, "net", ProcDirType::SysNetDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_sys_net_core_directory(&net_dir)?;
        
        self.create_sys_net_ipv4_directory(&net_dir)?;
        
        // info!("Created /proc/sys/net directory");
        Ok(())
    }

    fn create_sys_net_core_directory(&self, net_dir: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let core_dir = self.create_dir(net_dir, "core", ProcDirType::SysNetCoreDir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_sys_config_file(&core_dir, "rmem_default", ProcSysConfigType::NetCoreRmemDefault,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&core_dir, "rmem_max", ProcSysConfigType::NetCoreRmemMax,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&core_dir, "wmem_default", ProcSysConfigType::NetCoreWmemDefault,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&core_dir, "wmem_max", ProcSysConfigType::NetCoreWmemMax,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&core_dir, "somaxconn", ProcSysConfigType::NetCoreSomaxconn,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&core_dir, "netdev_max_backlog", ProcSysConfigType::NetCoreNetdevMaxBacklog,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        // info!("Created /proc/sys/net/core directory");
        Ok(())
    }

    fn create_sys_net_ipv4_directory(&self, net_dir: &Arc<KernFSInode>) -> Result<(), SystemError> {
        let ipv4_dir = self.create_dir(net_dir, "ipv4", ProcDirType::SysNetIpv4Dir,
            ModeType::S_IFDIR | ModeType::from_bits_truncate(0o555))?;
        
        self.create_sys_config_file(&ipv4_dir, "ip_forward", ProcSysConfigType::NetIpv4IpForward,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&ipv4_dir, "tcp_syncookies", ProcSysConfigType::NetIpv4TcpSyncookies,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&ipv4_dir, "tcp_timestamps", ProcSysConfigType::NetIpv4TcpTimestamps,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&ipv4_dir, "tcp_window_scaling", ProcSysConfigType::NetIpv4TcpWindowScaling,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&ipv4_dir, "tcp_keepalive_time", ProcSysConfigType::NetIpv4TcpKeepaliveTime,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        self.create_sys_config_file(&ipv4_dir, "tcp_fin_timeout", ProcSysConfigType::NetIpv4TcpFinTimeout,
            ModeType::S_IFREG | ModeType::from_bits_truncate(0o644))?; // 可读写
        
        // info!("Created /proc/sys/net/ipv4 directory");
        Ok(())
    }

    pub fn create_sys_config_file(
        &self,
        parent: &Arc<KernFSInode>,
        name: &str,
        config_type: ProcSysConfigType,
        mode: ModeType,
    ) -> Result<Arc<KernFSInode>, SystemError> {
        let private_data = ProcFSKernPrivateData::SysConfig(config_type);
        
        let callback: Option<&dyn crate::filesystem::kernfs::callback::KernFSCallback> = if mode.contains(ModeType::S_IWUSR) && mode.contains(ModeType::S_IRUSR) {
            // 可读写文件
            Some(&PROCFS_CALLBACK_RW)
        } else if mode.contains(ModeType::S_IWUSR) {
            // 只写文件
            Some(&PROCFS_CALLBACK_WO)
        } else {
            // 只读文件
            Some(&PROCFS_CALLBACK_RO)
        };

        parent.add_file(
            name.to_string(),
            mode,
            Some(4096),
            Some(KernInodePrivateData::ProcFS(private_data)),
            callback,
        )
    }
}
