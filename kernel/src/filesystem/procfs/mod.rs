pub mod data {
    pub mod system_info;
    pub mod process_info;
    pub mod sys_config;

    pub use system_info::{ProcSystemInfoType, get_system_info};
    pub use process_info::{ProcProcessInfoType, get_process_info};
    pub use sys_config::{ProcSysConfigType, get_sys_config, set_sys_config};
}

pub mod entries {
    pub mod dir;
    pub mod pid_dir;
}


pub mod file;
pub mod kmsg;
pub mod log;
pub mod syscall;
pub mod dynamic_pid_lookup;
pub mod registry;
pub mod fs;
pub mod proc_root_inode;

// 对外导出：核心类型与对外 API 从 fs.rs 暴露
pub use fs::{ProcFS, procfs_init, mount_proc_current_ns, procfs_register_pid, procfs_unregister_pid};

// 注册为可挂载文件系统名称 "proc"
crate::register_mountable_fs!(ProcFS, PROCFS_MAKER, "proc");