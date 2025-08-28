use alloc::{string::{String, ToString}, vec::Vec};
use system_error::SystemError;

use crate::{
    time::timer::clock,
    mm::MemoryManagementArch,
    arch::MMArch,
    smp::cpu::ProcessorId,
    process::ProcessManager,
    driver::base::device::device_number::DeviceNumber,
    filesystem::vfs::IndexNode,
    init::version_info,
};

use super::ProcFSKernPrivateData;

#[derive(Debug, Clone, Copy)]
pub enum ProcSystemInfoType {
    Version,
    CpuInfo,
    MemInfo,
    Uptime,
    LoadAvg,
    Stat,
    Interrupts,
    Devices,
    FileSystems,
    Mounts,
    CmdLine,
}

pub fn get_system_info(info_type: ProcSystemInfoType) -> Result<String, SystemError> {
        match info_type {
            // 构建版本信息，迁移自proc_version.rs
            ProcSystemInfoType::Version => {
                let info = version_info::get_kernel_build_info();
                
                Ok(format!(
                    "Linux version {} ({}@{}) ({}, {}) {}\n",
                    info.release,
                    info.build_user,
                    info.build_host,
                    info.compiler_info,
                    info.linker_info,
                    info.version
                ))
            }
            ProcSystemInfoType::CpuInfo => get_cpu_info(),
            ProcSystemInfoType::MemInfo => get_memory_info(),
            ProcSystemInfoType::Uptime => get_uptime_info(),
            ProcSystemInfoType::LoadAvg => get_loadavg_info(),
            ProcSystemInfoType::Stat => get_stat_info(),
            ProcSystemInfoType::Interrupts => get_interrupts_info(),
            ProcSystemInfoType::Devices => get_devices_info(),
            ProcSystemInfoType::FileSystems => get_filesystems_info(),
            ProcSystemInfoType::Mounts => get_mounts_info(),
            ProcSystemInfoType::CmdLine => get_cmdline_info(),
        }
}

/// 获取CPU信息
fn get_cpu_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("cpu_info:\n");
    Ok(result)        
}

/// 获取内存信息
fn get_memory_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("memiry_info:\n");
    Ok(result)         
}

/// 获取系统运行时间
fn get_uptime_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("uptime_info:\n");
    Ok(result)     
}

/// 获取系统负载
fn get_loadavg_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("loadavg_info:\n");
    Ok(result)     
}

/// 获取系统统计信息
fn get_stat_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("stat_info:\n");
    Ok(result)     
}

/// 获取中断信息
fn get_interrupts_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("interrupts_info:\n");
    Ok(result)     
}

/// 获取设备信息
fn get_devices_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("devices_info:\n");
    Ok(result)     
}

/// 获取文件系统信息
fn get_filesystems_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("filesystems_info:\n");
    Ok(result)
}

/// 获取挂载信息 - 迁移自proc_mounts.rs
fn get_mounts_info() -> Result<String, SystemError> {
    let mntns = ProcessManager::current_mntns();
    let mounts = mntns.mount_list().clone_inner();

    let mut lines = Vec::with_capacity(mounts.len());
    let mut cap = 0;
    for (mp, mfs) in mounts {
        let mut line = String::new();
        let fs_type = mfs.fs_type();
        let source = match fs_type {
            // 特殊文件系统，直接显示文件系统名称
            "devfs" | "devpts" | "sysfs" | "procfs" | "tmpfs" | "ramfs" | "rootfs"
            | "debugfs" | "configfs" => fs_type.to_string(),
            // 其他文件系统，尝试显示挂载设备名称
            _ => {
                if let Some(s) = mfs.self_mountpoint() {
                    // 尝试从挂载点获取设备名称
                    if let Some(device_name) = s.dname().ok().map(|d| d.to_string()) {
                        device_name
                    } else {
                        // 如果获取不到设备名称，使用绝对路径
                        s.absolute_path().unwrap_or("unknown".to_string())
                    }
                } else {
                    // 没有挂载点信息，使用文件系统类型
                    fs_type.to_string()
                }
            }
        };

        line.push_str(&format!("{source} {m} {fs_type}", m = mp.as_str()));

        line.push(' ');
        line.push_str(&mfs.mount_flags().options_string());

        line.push_str(" 0 0\n");
        cap += line.len();
        lines.push(line);
    }

    let mut content = String::with_capacity(cap);
    for line in lines {
        content.push_str(&line);
    }

    Ok(content)
}

/// 获取内核启动参数
fn get_cmdline_info() -> Result<String, SystemError> {
    let mut result = String::new();
    result.push_str("cmdline_info:\n");
    Ok(result)
}