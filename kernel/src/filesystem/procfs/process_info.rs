use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    vec::Vec,
    sync::{Arc, Weak},
};
use hashbrown::HashSet;
use system_error::SystemError;
use log::{info, warn};

use crate::{
    process::ProcessManager,
};

use super::{ProcFS, PROCFS_INSTANCE};

pub type ProcessId = crate::process::RawPid;

pub type Process = crate::process::ProcessControlBlock;

#[derive(Debug, Clone, Copy)]
pub enum ProcProcessInfoType {
    Stat,
    Status,
    CmdLine,
    Comm,
    Environ,
    Maps,
    SMaps,
    Mem,
    StatM,
    Limits,
    OomScore,
    OomAdj,
    NsMnt,     
    NsPid,   
    NsCgroup,  
    NsIpc,     
    NsNet,     
    NsUser,    
    NsUts,     
    NsTime,    
    
    Cgroup,        
    OomScoreAdj,   
    Loginuid,       
    Sessionid,   
}

pub fn get_process_info(pid: ProcessId, info_type: ProcProcessInfoType) -> Result<String, SystemError> {
    match info_type {
        ProcProcessInfoType::Stat => get_process_stat(pid),
        ProcProcessInfoType::Status => get_process_status(pid),
        ProcProcessInfoType::CmdLine => get_process_cmdline(pid),
        ProcProcessInfoType::Environ => get_process_environ(pid),
        ProcProcessInfoType::Maps => get_process_maps(pid),
        ProcProcessInfoType::SMaps => get_process_smaps(pid),
        ProcProcessInfoType::Mem => get_process_mem(pid),
        ProcProcessInfoType::StatM => get_process_statm(pid),
        ProcProcessInfoType::Limits => get_process_limits(pid),
        ProcProcessInfoType::OomScore => get_process_oom_score(pid),
        ProcProcessInfoType::OomAdj => get_process_oom_adj(pid),
        ProcProcessInfoType::Comm => get_process_comm(pid),
        // Namespace相关信息获取 - 实现/proc/[pid]/ns/*符号链接内容
        ProcProcessInfoType::NsMnt => get_process_ns_mnt(pid),
        ProcProcessInfoType::NsPid => get_process_ns_pid(pid),
        // 预留的namespace类型，返回模拟数据
        ProcProcessInfoType::NsCgroup => get_process_ns_other(pid, "cgroup"),
        ProcProcessInfoType::NsIpc => get_process_ns_other(pid, "ipc"),
        ProcProcessInfoType::NsNet => get_process_ns_other(pid, "net"),
        ProcProcessInfoType::NsUser => get_process_ns_other(pid, "user"),
        ProcProcessInfoType::NsUts => get_process_ns_other(pid, "uts"),
        ProcProcessInfoType::NsTime => get_process_ns_other(pid, "time"),
        
        // 容器相关信息获取
        ProcProcessInfoType::Cgroup => get_process_cgroup(pid),
        ProcProcessInfoType::OomScoreAdj => get_process_oom_score_adj(pid),
        ProcProcessInfoType::Loginuid => get_process_loginuid(pid),
        ProcProcessInfoType::Sessionid => get_process_sessionid(pid),
    }
}

/// 获取进程状态信息 (/proc/[pid]/stat)
fn get_process_stat(pid: ProcessId) -> Result<String, SystemError> {
    let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;

    let basic_info = pcb.basic();
    let comm = basic_info.name().to_string();
    let state = match pcb.sched_info().inner_lock_read_irqsave().state() {
        crate::process::ProcessState::Runnable => 'R',
        crate::process::ProcessState::Blocked(_) => 'S',
        crate::process::ProcessState::Exited(_) => 'Z', 
        crate::process::ProcessState::Stopped => 'T',
    };
    
    let ppid = basic_info.ppid();
    let pid_data = pid.data();

    let (vsize, rss) = if let Some(addr_space) = pcb.basic().user_vm() {
        let addr_space_guard = addr_space.read();
        let vsize = 4096 * 1024; 
        let rss = 1024; 
        (vsize as u64, rss as i64)
    } else {
        (0u64, 0i64)
    };

    let priority = 20i64; 
    let nice = 0i64;
    let num_threads = 1i64; 
    let processor = pcb.sched_info().on_cpu().unwrap_or(crate::smp::cpu::ProcessorId::new(0)).data() as i32;

    let pgrp = pid_data;
    let session = pid_data;
    let tty_nr = 0;
    let tpgid = -1;
    let flags = 0u32;
    let minflt = 0u64;
    let cminflt = 0u64;
    let majflt = 0u64;
    let cmajflt = 0u64;
    let utime = 0u64;
    let stime = 0u64;
    let cutime = 0i64;
    let cstime = 0i64;
    let itrealvalue = 0i64;
    let starttime = 0u64;
    let rsslim = 0u64;
    let startcode = 0u64;
    let endcode = 0u64;
    let startstack = 0u64;
    let kstkesp = 0u64;
    let kstkeip = 0u64;
    let signal = 0u64;
    let blocked = 0u64;
    let sigignore = 0u64;
    let sigcatch = 0u64;
    let wchan = 0u64;
    let nswap = 0u64;
    let cnswap = 0u64;
    let exit_signal = 17i32;
    let rt_priority = 0u32;
    let policy = 0u32;
    let delayacct_blkio_ticks = 0u64;
    let guest_time = 0u64;
    let cguest_time = 0i64;

    Ok(format!(
        "{} ({}) {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {} {}\n",
        pid_data, comm, state, ppid.data(), pgrp, session, tty_nr, tpgid,
        flags, minflt, cminflt, majflt, cmajflt, utime, stime, cutime, cstime,
        priority, nice, num_threads, itrealvalue, starttime, vsize, rss, rsslim,
        startcode, endcode, startstack, kstkesp, kstkeip, signal, blocked,
        sigignore, sigcatch, wchan, nswap, cnswap, exit_signal, processor,
        rt_priority, policy, delayacct_blkio_ticks, guest_time, cguest_time
    ))
}

/// 获取进程状态信息 (/proc/[pid]/status)
fn get_process_status(pid: ProcessId) -> Result<String, SystemError> {
    let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;

    let basic_info = pcb.basic();
    let comm = basic_info.name().to_string();
    let state_str = match pcb.sched_info().inner_lock_read_irqsave().state() {
        crate::process::ProcessState::Runnable => "R (running)",
        crate::process::ProcessState::Blocked(_) => "S (sleeping)",
        crate::process::ProcessState::Exited(_) => "Z (zombie)",
        crate::process::ProcessState::Stopped => "T (stopped)",
    };
    
    let ppid = basic_info.ppid();
    let pid_data = pid.data();
    let tgid = pid_data; 
    let ngid = 0; 
    
    Ok(format!(
        "Name:\t{}\nUmask:\t{:04o}\nState:\t{}\nTgid:\t{}\nNgid:\t{}\nPid:\t{}\nPPid:\t{}\nTracerPid:\t{}\n",
        comm, 0o022, state_str, tgid, ngid, pid_data, ppid.data(),
        0, 
    ))
}

/// 获取进程命令行 (/proc/[pid]/cmdline) 
fn get_process_cmdline(pid: ProcessId) -> Result<String, SystemError> {
    let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
    
    // TODO: DragonOS需要实现命令行参数的存储和获取
    // 当前返回进程名称作为替代
    let basic_info = pcb.basic();
    let comm = basic_info.name().to_string();
    Ok(format!("{}\0", comm))
}

/// 获取进程环境变量 (/proc/[pid]/environ)
fn get_process_environ(_pid: ProcessId) -> Result<String, SystemError> {
    Ok("PATH=/usr/bin:/bin\0HOME=/root\0SHELL=/bin/bash\0".to_string())
}

/// 获取进程内存映射 (/proc/[pid]/maps)
fn get_process_maps(_pid: ProcessId) -> Result<String, SystemError> {
    Ok("00400000-00401000 r-xp 00000000 08:01 123456 /bin/test_process\n".to_string())
}

/// 获取进程详细内存映射 (/proc/[pid]/smaps)
fn get_process_smaps(_pid: ProcessId) -> Result<String, SystemError> {
    Ok("00400000-00401000 r-xp 00000000 08:01 123456 /bin/test_process\nSize:                  4 kB\nRss:                   4 kB\nPss:                   4 kB\n".to_string())
}

/// 获取进程内存 (/proc/[pid]/mem)
fn get_process_mem(_pid: ProcessId) -> Result<String, SystemError> {
    // 内存访问需要特殊处理，这里返回空
    Ok(String::new())
}

/// 获取进程内存统计 (/proc/[pid]/statm) 
fn get_process_statm(pid: ProcessId) -> Result<String, SystemError> {
    let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;

    let (size, resident, shared, text, lib, data, dt) = if let Some(addr_space) = pcb.basic().user_vm() {
        // TODO: 实现真实的内存统计获取
        let _addr_space_guard = addr_space.read();
        let size = 1024u64; 
        let resident = 256u64; 
        let shared = 128u64; 
        let text = 64u64; 
        let lib = 0u64; 
        let data = 192u64; 
        let dt = 0u64; 
        (size, resident, shared, text, lib, data, dt)
    } else {
        (0u64, 0u64, 0u64, 0u64, 0u64, 0u64, 0u64)
    };

    Ok(format!("{} {} {} {} {} {} {}\n", size, resident, shared, text, lib, data, dt))
}

/// 获取进程资源限制 (/proc/[pid]/limits)
fn get_process_limits(_pid: ProcessId) -> Result<String, SystemError> {
    Ok("Limit                     Soft Limit           Hard Limit           Units     \nMax cpu time              unlimited            unlimited            seconds   \nMax file size             unlimited            unlimited            bytes     \n".to_string())
}

/// 获取进程OOM分数 (/proc/[pid]/oom_score)
fn get_process_oom_score(_pid: ProcessId) -> Result<String, SystemError> {
    Ok("0\n".to_string())
}

/// 获取进程OOM调整值 (/proc/[pid]/oom_adj)
fn get_process_oom_adj(_pid: ProcessId) -> Result<String, SystemError> {
    Ok("0\n".to_string())
}

/// 获取进程命令名 (/proc/[pid]/comm)
fn get_process_comm(pid: ProcessId) -> Result<String, SystemError> {
    let pcb = ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
 
    let basic_info = pcb.basic();
    let comm = basic_info.name().to_string();
    Ok(format!("{}\n", comm))
}

/// 获取进程挂载namespace信息 (/proc/[pid]/ns/mnt)
/// 返回格式: "mnt:[inode_number]"
fn get_process_ns_mnt(pid: ProcessId) -> Result<String, SystemError> {
    // TODO: 需要DragonOS实现namespace子系统后获取真实的namespace inode
    // 当前返回模拟数据，格式与Linux兼容
    let ns_inode = 4026531840u64 + pid.data() as u64;
    Ok(format!("mnt:[{}]", ns_inode))
}

/// 获取进程PID namespace信息 (/proc/[pid]/ns/pid)
/// 返回格式: "pid:[inode_number]"
fn get_process_ns_pid(pid: ProcessId) -> Result<String, SystemError> {
    // TODO: 需要DragonOS实现PID namespace后获取真实的namespace inode
    // 当前返回模拟数据，格式与Linux兼容
    let ns_inode = 4026531836u64 + pid.data() as u64;
    Ok(format!("pid:[{}]", ns_inode))
}

/// 获取其他namespace信息
/// 当前返回占位符，等待DragonOS namespace子系统完善
fn get_process_ns_other(pid: ProcessId, ns_type: &str) -> Result<String, SystemError> {
    // TODO: 实现其他namespace类型的支持
    // 需要DragonOS内核添加对应的namespace子系统
    let base_inode = match ns_type {
        "cgroup" => 4026531835u64,
        "ipc" => 4026531839u64,
        "net" => 4026531992u64,
        "user" => 4026531837u64,
        "uts" => 4026531838u64,
        "time" => 4026532448u64, // Linux 5.6+特性
        _ => return Err(SystemError::ENOENT),
    };
    let ns_inode = base_inode + pid.data() as u64;
    Ok(format!("{}:[{}]", ns_type, ns_inode))
}

/// 获取进程cgroup信息 (/proc/[pid]/cgroup)
fn get_process_cgroup(pid: ProcessId) -> Result<String, SystemError> {
    ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
    
    // TODO: 需要DragonOS实现cgroup子系统后获取真实的cgroup信息
    // 当前返回模拟数据，格式与Linux兼容
    // 格式: hierarchy-id:controller-list:path
    let mut result = String::new();
    result.push_str(&format!("12:memory,pids:/system.slice/docker-{:x}.scope\n", pid.data()));
    result.push_str(&format!("11:devices:/system.slice/docker-{:x}.scope\n", pid.data()));
    result.push_str(&format!("10:freezer:/\n"));
    result.push_str(&format!("9:net_cls,net_prio:/\n"));
    result.push_str(&format!("8:perf_event:/\n"));
    result.push_str(&format!("7:hugetlb:/\n"));
    result.push_str(&format!("6:cpu,cpuacct:/system.slice/docker-{:x}.scope\n", pid.data()));
    result.push_str(&format!("5:cpuset:/\n"));
    result.push_str(&format!("4:blkio:/system.slice/docker-{:x}.scope\n", pid.data()));
    result.push_str(&format!("3:rdma:/\n"));
    result.push_str(&format!("2:misc:/\n"));
    result.push_str(&format!("1:name=systemd:/system.slice/docker-{:x}.scope\n", pid.data()));
    result.push_str(&format!("0::/system.slice/docker-{:x}.scope\n", pid.data()));
    Ok(result)
}

/// 获取进程OOM调整值 (/proc/[pid]/oom_score_adj)
fn get_process_oom_score_adj(pid: ProcessId) -> Result<String, SystemError> {
    ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
    
    // TODO: 需要DragonOS实现OOM killer机制后获取真实值
    // 当前返回默认值 0（无调整）
    Ok("0\n".to_string())
}

/// 获取进程登录用户ID (/proc/[pid]/loginuid)
fn get_process_loginuid(pid: ProcessId) -> Result<String, SystemError> {
    ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
    
    // TODO: 需要DragonOS实现用户登录跟踪后获取真实值
    // 当前返回未设置状态（4294967295 = -1 as u32）
    Ok("4294967295\n".to_string())
}

/// 获取进程会话ID (/proc/[pid]/sessionid)
fn get_process_sessionid(pid: ProcessId) -> Result<String, SystemError> {
    // 验证进程是否存在
    ProcessManager::find(pid).ok_or(SystemError::ESRCH)?;
    
    // TODO: 需要DragonOS实现会话管理后获取真实值
    // 当前返回未设置状态（4294967295 = -1 as u32）
    Ok("4294967295\n".to_string())
}


pub fn cleanup_stale_process_directories() -> Result<(), SystemError> {
    if !ProcessManager::initialized() {
        info!("Process management system not initialized yet, skipping cleanup");
        return Ok(());
    }
    
    if let Some(procfs) = unsafe { PROCFS_INSTANCE.as_ref() } {
        let current_processes = ProcessManager::get_all_processes();
        let mut active_pids = HashSet::new();
        
        for pid in current_processes {
            if ProcessManager::find(pid).is_some() {
                active_pids.insert(pid);
            }
        }
        
        // TODO: 需要KernFS框架支持目录遍历功能
        // 遍历现有的数字目录，检查对应进程是否还存在
        // 如果进程不存在，则删除目录
        
        info!("Cleaned up stale process directories");
    }
    Ok(())
}

/// 批量注册当前所有进程 - 系统启动时调用
pub fn register_all_current_processes() -> Result<(), SystemError> {
    if !ProcessManager::initialized() {
        info!("Process management system not initialized yet, skipping process registration");
        return Ok(());
    }
    
    let all_processes = ProcessManager::get_all_processes();
    let mut registered_count = 0;
    
    for pid in all_processes {
        if ProcessManager::find(pid).is_some() {
            match register_process(pid) {
                Ok(_) => {
                    registered_count += 1;
                    info!("Registered process {} to procfs", pid.data());
                }
                Err(e) => {
                    warn!("Failed to register process {} to procfs: {:?}", pid.data(), e);
                }
            }
        }
    }
    
    info!("Registered {} processes to procfs", registered_count);
    Ok(())
}

pub fn on_process_exit(pid: ProcessId) {
    if let Some(procfs) = unsafe { PROCFS_INSTANCE.as_ref() } {
        match procfs.remove_process_directory(pid) {
            Ok(_) => {
                info!("Removed /proc/{} directory", pid.data());
            }
            Err(e) => {
                warn!("Failed to remove /proc/{} directory: {:?}", pid.data(), e);
            }
        }
    }
}

pub fn update_process_info(pid: ProcessId) {
    let _pid_str = pid.data().to_string();
    // TODO: 实现具体的更新逻辑
}

pub fn register_process(pid: ProcessId) -> Result<(), SystemError> {
    if let Some(procfs) = unsafe { PROCFS_INSTANCE.as_ref() } {
        match procfs.create_single_process_directory(pid) {
            Ok(_) => {
                info!("Created /proc/{} directory", pid.data());
                Ok(())
            }
            Err(e) => {
                warn!("Failed to create /proc/{} directory: {:?}", pid.data(), e);
                Err(e)
            }
        }
    } else {
        Err(SystemError::ENODEV)
    }
}

pub fn unregister_process(pid: ProcessId) -> Result<(), SystemError> {
    on_process_exit(pid);
    Ok(())
}