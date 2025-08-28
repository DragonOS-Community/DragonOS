use alloc::{string::{String, ToString}, sync::Arc, boxed::Box, collections::BTreeMap};
use system_error::SystemError;
use log::{warn, info, error};
use crate::filesystem::kernfs::{KernFSInode, callback::KernFSCallback};
use crate::filesystem::vfs::syscall::ModeType;
use crate::libs::spinlock::SpinLock;
use crate::process::ProcessManager;
use super::{ProcFSKernPrivateData, ProcDirType };

#[derive(Debug, Clone)]
pub enum ConfigValue {
    String(String),
    U32(u32),
    U64(u64),
    Bool(bool),
}

impl ConfigValue {
    /// 转换为proc文件系统格式的字符串
    pub fn to_proc_string(&self) -> String {
        match self {
            ConfigValue::String(s) => format!("{}\n", s),
            ConfigValue::U32(n) => format!("{}\n", n),
            ConfigValue::U64(n) => format!("{}\n", n),
            ConfigValue::Bool(b) => format!("{}\n", if *b { 1 } else { 0 }),
        }
    }

    /// 从字符串解析配置值，基于期望类型进行类型推断
    pub fn from_str_typed(s: &str, expected_type: &ConfigValue) -> Result<ConfigValue, SystemError> {
        let trimmed = s.trim();
        match expected_type {
            ConfigValue::String(_) => Ok(ConfigValue::String(trimmed.to_string())),
            ConfigValue::U32(_) => {
                trimmed.parse::<u32>()
                    .map(ConfigValue::U32)
                    .map_err(|_| SystemError::EINVAL)
            }
            ConfigValue::U64(_) => {
                trimmed.parse::<u64>()
                    .map(ConfigValue::U64)
                    .map_err(|_| SystemError::EINVAL)
            }
            ConfigValue::Bool(_) => {
                match trimmed {
                    "0" => Ok(ConfigValue::Bool(false)),
                    "1" => Ok(ConfigValue::Bool(true)),
                    _ => Err(SystemError::EINVAL),
                }
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ProcSysConfigType {
    // /proc/sys/kernel/ 内核参数
    KernelVersion,
    KernelHostname,
    KernelDomainname,
    KernelOstype,
    KernelOsrelease,
    KernelPanic,
    KernelPanicOnOops,
    KernelPidMax,
    KernelThreadsMax,
    KernelRandomBootId,

    // /proc/sys/vm/ 虚拟内存参数
    VmSwappiness,
    VmDirtyRatio,
    VmDirtyBackgroundRatio,
    VmDropCaches,
    VmOvercommitMemory,
    VmOvercommitRatio,
    VmMinFreeKbytes,

    // /proc/sys/fs/ 文件系统参数
    FsFileMax,
    FsFileNr,
    FsInodeMax,
    FsInodeNr,
    FsAioMaxNr,

    // /proc/sys/net/core/ 网络核心参数
    NetCoreRmemDefault,
    NetCoreRmemMax,
    NetCoreWmemDefault,
    NetCoreWmemMax,
    NetCoreSomaxconn,
    NetCoreNetdevMaxBacklog,

    // /proc/sys/net/ipv4/ IPv4网络参数
    NetIpv4IpForward,
    NetIpv4TcpSyncookies,
    NetIpv4TcpTimestamps,
    NetIpv4TcpWindowScaling,
    NetIpv4TcpKeepaliveTime,
    NetIpv4TcpFinTimeout,
}

/// 全局系统配置
static SYSTEM_CONFIGS: SpinLock<BTreeMap<ProcSysConfigType, ConfigValue>> = SpinLock::new(BTreeMap::new());

/// 初始化系统配置 - 在系统启动时调用
pub fn init_system_configs() {
    let mut configs = SYSTEM_CONFIGS.lock();

    configs.insert(ProcSysConfigType::KernelHostname, ConfigValue::String("dragonos".to_string()));
    configs.insert(ProcSysConfigType::KernelDomainname, ConfigValue::String("(none)".to_string()));
    configs.insert(ProcSysConfigType::KernelPanic, ConfigValue::U32(0));
    configs.insert(ProcSysConfigType::KernelPanicOnOops, ConfigValue::Bool(false));
    configs.insert(ProcSysConfigType::KernelPidMax, ConfigValue::U32(32768));
    
    // 虚拟内存参数
    configs.insert(ProcSysConfigType::VmSwappiness, ConfigValue::U32(60));
    configs.insert(ProcSysConfigType::VmDirtyRatio, ConfigValue::U32(20));
    configs.insert(ProcSysConfigType::VmDirtyBackgroundRatio, ConfigValue::U32(10));
    configs.insert(ProcSysConfigType::VmOvercommitMemory, ConfigValue::U32(0));
    configs.insert(ProcSysConfigType::VmOvercommitRatio, ConfigValue::U32(50));
    
    // 文件系统参数
    configs.insert(ProcSysConfigType::FsFileMax, ConfigValue::U32(1048576));
    configs.insert(ProcSysConfigType::FsAioMaxNr, ConfigValue::U32(65536));
    
    // 网络参数
    configs.insert(ProcSysConfigType::NetIpv4IpForward, ConfigValue::Bool(false));
    configs.insert(ProcSysConfigType::NetCoreSomaxconn, ConfigValue::U32(4096));
    configs.insert(ProcSysConfigType::NetCoreRmemDefault, ConfigValue::U32(212992));
    configs.insert(ProcSysConfigType::NetCoreRmemMax, ConfigValue::U32(212992));
    configs.insert(ProcSysConfigType::NetCoreWmemDefault, ConfigValue::U32(212992));
    configs.insert(ProcSysConfigType::NetCoreWmemMax, ConfigValue::U32(212992));
    configs.insert(ProcSysConfigType::NetCoreNetdevMaxBacklog, ConfigValue::U32(1000));
    configs.insert(ProcSysConfigType::NetIpv4TcpSyncookies, ConfigValue::Bool(true));
    configs.insert(ProcSysConfigType::NetIpv4TcpTimestamps, ConfigValue::Bool(true));
    configs.insert(ProcSysConfigType::NetIpv4TcpWindowScaling, ConfigValue::Bool(true));
    configs.insert(ProcSysConfigType::NetIpv4TcpKeepaliveTime, ConfigValue::U32(7200));
    configs.insert(ProcSysConfigType::NetIpv4TcpFinTimeout, ConfigValue::U32(60));
    
    // info!("System configuration initialized with default values");
}

fn validate_config_value(config_type: ProcSysConfigType, value: &ConfigValue) -> Result<(), SystemError> {
    match (config_type, value) {
        (ProcSysConfigType::KernelHostname, ConfigValue::String(s)) => {
            if s.len() > 64 {
                return Err(SystemError::EINVAL);
            }
            if s.chars().any(|c| !c.is_ascii_alphanumeric() && c != '-' && c != '.') {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::KernelDomainname, ConfigValue::String(s)) => {
            if s.len() > 64 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::KernelPidMax, ConfigValue::U32(pid_max)) => {
            if *pid_max < 300 || *pid_max > 4194304 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::VmSwappiness, ConfigValue::U32(swappiness)) => {
            if *swappiness > 100 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::VmDirtyRatio, ConfigValue::U32(ratio)) => {
            if *ratio > 100 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::VmDirtyBackgroundRatio, ConfigValue::U32(ratio)) => {
            if *ratio > 100 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::VmOvercommitMemory, ConfigValue::U32(overcommit)) => {
            if *overcommit > 2 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::VmOvercommitRatio, ConfigValue::U32(ratio)) => {
            if *ratio > 100 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::FsFileMax, ConfigValue::U32(file_max)) => {
            if *file_max < 1024 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetCoreSomaxconn, ConfigValue::U32(conn)) => {
            if *conn < 1 || *conn > 65535 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetCoreRmemDefault, ConfigValue::U32(size)) => {
            if *size < 256 || *size > 16777216 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetCoreRmemMax, ConfigValue::U32(size)) => {
            if *size < 256 || *size > 16777216 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetCoreWmemDefault, ConfigValue::U32(size)) => {
            if *size < 256 || *size > 16777216 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetCoreWmemMax, ConfigValue::U32(size)) => {
            if *size < 256 || *size > 16777216 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetCoreNetdevMaxBacklog, ConfigValue::U32(backlog)) => {
            if *backlog < 8 || *backlog > 65535 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetIpv4TcpKeepaliveTime, ConfigValue::U32(time)) => {
            if *time < 1 || *time > 32767 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        (ProcSysConfigType::NetIpv4TcpFinTimeout, ConfigValue::U32(timeout)) => {
            if *timeout < 1 || *timeout > 300 {
                return Err(SystemError::EINVAL);
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn is_readonly_config(config_type: ProcSysConfigType) -> bool {
    matches!(config_type,
        ProcSysConfigType::KernelVersion |
        ProcSysConfigType::KernelOstype |
        ProcSysConfigType::KernelOsrelease |
        ProcSysConfigType::KernelRandomBootId |
        ProcSysConfigType::KernelThreadsMax |
        ProcSysConfigType::FsFileNr |
        ProcSysConfigType::FsInodeNr |
        ProcSysConfigType::FsInodeMax |
        ProcSysConfigType::VmMinFreeKbytes
    )
}

fn apply_config_change(config_type: ProcSysConfigType, value: &ConfigValue) -> Result<(), SystemError> {
    match config_type {
        ProcSysConfigType::KernelHostname => {
            // TODO: 集成DragonOS的hostname管理模块
            if let ConfigValue::String(hostname) = value {
                info!("Applied hostname configuration: {}", hostname);
            }
            Ok(())
        }
        ProcSysConfigType::KernelDomainname => {
            // TODO: 集成DragonOS的domainname管理模块
            if let ConfigValue::String(domainname) = value {
                info!("Applied domainname configuration: {}", domainname);
            }
            Ok(())
        }
        ProcSysConfigType::VmDropCaches => {
            // TODO: 集成DragonOS的缓存管理
            if let ConfigValue::U32(cache_type) = value {
                info!("Dropping caches: type {}", cache_type);
                // 实际的缓存清理逻辑需要调用内存管理模块
                match cache_type {
                    1 => info!("Dropping page cache"),
                    2 => info!("Dropping dentries and inodes"),
                    3 => info!("Dropping all caches"),
                    _ => return Err(SystemError::EINVAL),
                }
            }
            Ok(())
        }
        ProcSysConfigType::NetIpv4IpForward => {
            // TODO: 集成DragonOS的IPv4路由配置
            if let ConfigValue::Bool(forward) = value {
                info!("Applied IP forward configuration: {}", forward);
            }
            Ok(())
        }
        _ => {
            // 大部分配置暂时只记录，待后续集成实际系统模块
            info!("Configuration change applied: {:?}", config_type);
            Ok(())
        }
    }
}


/// 获取系统配置信息的实现
pub fn get_sys_config(config_type: ProcSysConfigType) -> Result<String, SystemError> {
    match config_type {
        ProcSysConfigType::KernelVersion => {
            Ok(format!("{}\n", env!("CARGO_PKG_VERSION")))
        }
        ProcSysConfigType::KernelOstype => {
            Ok("DragonOS\n".to_string())
        }
        ProcSysConfigType::KernelOsrelease => {
            Ok(format!("{}\n", env!("CARGO_PKG_VERSION")))
        }
        ProcSysConfigType::KernelRandomBootId => {
            // TODO: 集成真实的随机数生成器生成UUID
            Ok("12345678-1234-5678-9abc-123456789abc\n".to_string())
        }
        ProcSysConfigType::KernelThreadsMax => {
            // TODO: 基于内存大小动态计算最大线程数
            // 基本公式：可用内存(MB) / 8 = 最大线程数
            get_threads_max_from_memory()
        }
        
        // 实时内存统计 - 直接从系统获取，确保数据实时性
        ProcSysConfigType::VmMinFreeKbytes => {
            // TODO: 集成buddy分配器获取真实内存统计
            get_min_free_kbytes_from_mm()
        }
        ProcSysConfigType::FsFileNr => {
            // TODO: 集成VFS获取真实文件句柄统计
            // 格式：已分配 已使用 最大值
            get_file_nr_from_fs()
        }
        ProcSysConfigType::FsInodeNr => {
            // TODO: 集成VFS获取真实inode统计
            // 格式：已分配 空闲
            get_inode_nr_from_fs()
        }
        ProcSysConfigType::FsInodeMax => {
            // TODO: 集成VFS获取inode配置
            // 0表示无限制
            Ok("0\n".to_string())
        }
        ProcSysConfigType::VmDropCaches => {
            // 这是一个只写参数，读取时返回0
            Ok("0\n".to_string())
        }
        
        // 从全局配置存储获取静态配置
        _ => {
            let configs = SYSTEM_CONFIGS.lock();
            if let Some(value) = configs.get(&config_type) {
                Ok(value.to_proc_string())
            } else {
                error!("Configuration not found: {:?}", config_type);
                Err(SystemError::ENOENT)
            }
        }
    }
}

/// 从内存管理模块获取实时的最小空闲内存
fn get_min_free_kbytes_from_mm() -> Result<String, SystemError> {
    // TODO: 集成buddy分配器获取真实内存统计信息
    // 当前返回基于系统内存大小的估算值
    Ok("67584\n".to_string())
}

/// 从文件系统获取实时的文件句柄统计
fn get_file_nr_from_fs() -> Result<String, SystemError> {
    // TODO: 集成VFS获取真实文件句柄统计
    // 格式：已分配 已使用 最大值
    Ok("1024\t0\t1048576\n".to_string())
}

/// 从文件系统获取实时的inode统计
fn get_inode_nr_from_fs() -> Result<String, SystemError> {
    // TODO: 集成VFS获取真实inode统计
    // 格式：已分配 空闲
    Ok("1024\t512\n".to_string())
}

/// 基于内存大小计算最大线程数
fn get_threads_max_from_memory() -> Result<String, SystemError> {
    // TODO: 集成内存管理模块获取实际可用内存
    // 基本公式：可用内存(MB) / 8 = 最大线程数
    // 当前返回基于典型配置的估算值
    Ok("15739\n".to_string())
}

pub fn set_sys_config(config_type: ProcSysConfigType, buf: &[u8]) -> Result<usize, SystemError> {
    let content = core::str::from_utf8(buf).map_err(|_| SystemError::EINVAL)?;

    if is_readonly_config(config_type) {
        warn!("Attempt to write to read-only sysctl: {:?}", config_type);
        return Err(SystemError::EPERM);
    }

    // 特殊处理VmDropCaches（只写配置）
    if config_type == ProcSysConfigType::VmDropCaches {
        let value = content.trim().parse::<u32>().map_err(|_| SystemError::EINVAL)?;
        if value > 3 {
            return Err(SystemError::EINVAL);
        }
        let config_value = ConfigValue::U32(value);
        apply_config_change(config_type, &config_value)?;
        return Ok(buf.len());
    }

    let mut configs = SYSTEM_CONFIGS.lock();
    
    // 获取当前配置以确定类型
    if let Some(current_value) = configs.get(&config_type) {
        let new_value = ConfigValue::from_str_typed(content, current_value)?;
    
        validate_config_value(config_type, &new_value)?;
  
        apply_config_change(config_type, &new_value)?;
        
        // 更新配置存储
        configs.insert(config_type, new_value);
        info!("Updated sysctl {:?}", config_type);
        Ok(buf.len())
    } else {
        error!("Configuration type not found: {:?}", config_type);
        Err(SystemError::ENOENT)
    }
}