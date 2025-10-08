use system_error::SystemError;

use alloc::{string::String, vec::Vec};

use super::{
    fs::ProcFSInfo,
    entries::dir::ProcDirType,
    data::process_info::ProcessId,
};
use crate::filesystem::procfs::data::{
    ProcSystemInfoType, ProcProcessInfoType, ProcSysConfigType,
    get_system_info, get_process_info, get_sys_config, set_sys_config,
};
use crate::filesystem::kernfs::callback::{KernCallbackData, KernFSCallback};
use crate::filesystem::vfs::PollStatus;
use crate::process::ProcessManager;



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
    /// 挂载实例上下文（仅用于在inode私有数据中携带Weak<ProcFSInfo>）
    MountContext(alloc::sync::Weak<ProcFSInfo>),
}


impl ProcFSKernPrivateData {
    /// 统一的读取接口
    pub fn callback_read(&self, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        // use ::log::info;
        // info!("ProcFSKernPrivateData::callback_read called, type: {:?}", self);
        
        let content = match self {

            // 挂载上下文不是可读文件，返回不支持
            ProcFSKernPrivateData::MountContext(_weak_info) => {
                return Err(SystemError::ENOSYS);
            }

            // 匹配SystemInfo 
            ProcFSKernPrivateData::SystemInfo(info_type) => {
                // info!("Getting system info for type: {:?}", info_type);
                get_system_info(*info_type)?
            }

            // 匹配ProcessInfo
            ProcFSKernPrivateData::ProcessInfo(pid, info_type) => {
                // info!("Getting process info for PID: {}, type: {:?}", pid.data(), info_type);
                
                if ProcessManager::find(*pid).is_none() {
                    return Err(SystemError::ESRCH);
                }
                
                get_process_info(*pid, *info_type)?
            }

            // 匹配SysConfig
            ProcFSKernPrivateData::SysConfig(config_type) => {
                get_sys_config(*config_type)?
            }

            // 匹配Dir
            ProcFSKernPrivateData::Dir(dir_type) => {
                match dir_type {
                    
                    // 如果是进程目录
                    ProcDirType::ProcessDir(pid) => {
                        if ProcessManager::find(*pid).is_none() {
                            return Err(SystemError::ENOENT);
                        }
                        return Err(SystemError::EISDIR);
                    }

                    // 其他目录
                    _ => return Err(SystemError::EISDIR),
                }
            }

            // 匹配ProcessList
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




/// 只读文件回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackReadOnly;

impl KernFSCallback for ProcFSCallbackReadOnly {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, data: KernCallbackData, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        // use ::log::info;
        // info!("ProcFSCallbackReadOnly::read called, buf_len={}, offset={}", buf.len(), offset);
        let result = data.callback_read(buf, offset);
        // info!("ProcFSCallbackReadOnly::read result: {:?}", result);
        result
    }

    fn write(&self, _data: KernCallbackData, _buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ)
    }
}

/// 只写文件回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackWriteOnly;

impl KernFSCallback for ProcFSCallbackWriteOnly {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, _data: KernCallbackData, _buf: &mut [u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn write(&self, data: KernCallbackData, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        data.callback_write(buf, offset)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::WRITE)
    }
}

/// 读写文件回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackRW;

impl KernFSCallback for ProcFSCallbackRW {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, data: KernCallbackData, buf: &mut [u8], offset: usize) -> Result<usize, SystemError> {
        data.callback_read(buf, offset)
    }

    fn write(&self, data: KernCallbackData, buf: &[u8], offset: usize) -> Result<usize, SystemError> {
        data.callback_write(buf, offset)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::READ | PollStatus::WRITE)
    }
}

/// 空操作回调
#[derive(Debug)]
pub(super) struct ProcFSCallbackEmpty;

impl KernFSCallback for ProcFSCallbackEmpty {
    fn open(&self, _data: KernCallbackData) -> Result<(), SystemError> {
        Ok(())
    }

    fn read(&self, _data: KernCallbackData, _buf: &mut [u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn write(&self, _data: KernCallbackData, _buf: &[u8], _offset: usize) -> Result<usize, SystemError> {
        Err(SystemError::EPERM)
    }

    fn poll(&self, _data: KernCallbackData) -> Result<PollStatus, SystemError> {
        Ok(PollStatus::empty())
    }
}

// 全局回调实例
pub(super) static PROCFS_CALLBACK_RO: ProcFSCallbackReadOnly = ProcFSCallbackReadOnly;
pub(super) static PROCFS_CALLBACK_WO: ProcFSCallbackWriteOnly = ProcFSCallbackWriteOnly;
pub(super) static PROCFS_CALLBACK_RW: ProcFSCallbackRW = ProcFSCallbackRW;
#[allow(dead_code)]
pub(super) static PROCFS_CALLBACK_EMPTY: ProcFSCallbackEmpty = ProcFSCallbackEmpty;