use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

use crate::filesystem::{
    kernfs::{KernFSInode, callback::KernInodePrivateData},
    vfs::syscall::ModeType,
    procfs::ProcFS,
};
use super::super::file::{ProcFSKernPrivateData, PROCFS_CALLBACK_RO, PROCFS_CALLBACK_WO, PROCFS_CALLBACK_RW};
use super::super::data::{system_info::ProcSystemInfoType, sys_config::ProcSysConfigType, process_info::ProcessId};


#[derive(Debug, Clone, Copy)]
pub enum ProcDirType {
    Root,
    ProcessDir(ProcessId),
    SysDir,              
    SysKernelDir,        
    SysVmDir,           
    SysFsDir,           
    SysNetDir,           
    SysNetCoreDir,       
    SysNetIpv4Dir,       
    ProcessNsDir(ProcessId),  
}



impl ProcFS {
    



    pub fn create_system_files(&self, root: &Arc<KernFSInode>) -> Result<(), SystemError> {
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
        
        self.create_system_info_file(root, "kmsg", ProcSystemInfoType::Kmsg,
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
        // Linux风格：完全禁止在根目录下创建数字命名的目录（PID目录）
        // PID目录应该完全通过动态查找机制创建
        if name.parse::<u32>().is_ok() && parent.parent().is_none() {
            ::log::debug!("ProcFS: Refusing to statically create PID directory '{}' in root (should be dynamic)", name);
            return Err(SystemError::EACCES); // 让调用者知道应该使用动态机制
        }
        
        let private_data = ProcFSKernPrivateData::Dir(dir_type);
        
        parent.add_dir(
            name.to_string(),
            mode,
            Some(KernInodePrivateData::ProcFS(private_data)),
            None,
        )
    }



    pub fn create_sys_directory(&self, root: &Arc<KernFSInode>) -> Result<(), SystemError> {
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