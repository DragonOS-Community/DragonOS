//! 内存策略工具和定义

use system_error::SystemError;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use crate::mm::VirtAddr;

/// NUMA内存策略类型
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MempolicyMode {
    /// 默认策略 - 本地分配
    Default = 0,
    /// 绑定到特定节点
    Bind = 1,
    /// 跨节点交错分配
    Interleave = 2,
    /// 首选节点分配
    Preferred = 3,
    /// 本地节点分配
    Local = 4,
}

impl TryFrom<u32> for MempolicyMode {
    type Error = SystemError;

    fn try_from(value: u32) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(MempolicyMode::Default),
            1 => Ok(MempolicyMode::Bind),
            2 => Ok(MempolicyMode::Interleave),
            3 => Ok(MempolicyMode::Preferred),
            4 => Ok(MempolicyMode::Local),
            _ => Err(SystemError::EINVAL),
        }
    }
}

bitflags::bitflags! {
    /// get_mempolicy系统调用的标志
    pub struct MempolicyFlags: u32 {
        /// 获取有效策略
        const MPOL_F_NODE = 1 << 0;
        /// 获取特定地址的策略
        const MPOL_F_ADDR = 1 << 1;
        /// 获取特定内存区域的策略
        const MPOL_F_MEMS_ALLOWED = 1 << 2;
    }
}

/// 内存策略结构
#[derive(Debug, Clone)]
pub struct Mempolicy {
    /// 策略模式
    pub mode: MempolicyMode,
    /// 策略的节点掩码
    pub nodemask: u64,
    /// 首选节点（用于PREFERRED模式）
    pub preferred_node: Option<u32>,
}

impl Default for Mempolicy {
    fn default() -> Self {
        Self {
            mode: MempolicyMode::Default,
            nodemask: 0,
            preferred_node: None,
        }
    }
}

impl Mempolicy {
    /// 创建新的默认内存策略
    pub fn new_default() -> Self {
        Self::default()
    }

    /// 获取策略模式的u32值
    pub fn mode_as_u32(&self) -> u32 {
        self.mode as u32
    }

    /// 检查策略是否针对特定节点
    pub fn is_node_policy(&self) -> bool {
        matches!(self.mode, MempolicyMode::Bind | MempolicyMode::Preferred)
    }
}

/// 将策略信息写入用户空间
pub fn write_policy_to_user(
    policy_ptr: VirtAddr,
    mode: u32,
) -> Result<(), SystemError> {
    if !policy_ptr.is_null() {
        let mut writer = UserBufferWriter::new(
            policy_ptr.data() as *mut u32,
            core::mem::size_of::<u32>(),
            true,
        )?;
        writer.copy_one_to_user(&mode, 0)?;
    }
    Ok(())
}

/// 将节点掩码写入用户空间
pub fn write_nodemask_to_user(
    nmask_ptr: VirtAddr,
    nodemask: u64,
    maxnode: usize,
) -> Result<(), SystemError> {
    if !nmask_ptr.is_null() && maxnode > 0 {
        let bytes_to_write = (maxnode + 7) / 8; // 将位转换为字节
        let bytes_to_write = bytes_to_write.min(8); // 限制为u64大小
        
        let mut writer = UserBufferWriter::new(
            nmask_ptr.data() as *mut u8,
            bytes_to_write,
            true,
        )?;
        
        let mask_bytes = nodemask.to_ne_bytes();
        writer.copy_to_user(&mask_bytes[..bytes_to_write], 0)?;
    }
    Ok(())
}

/// 从用户空间读取节点掩码
pub fn _read_nodemask_from_user(
    nmask_ptr: VirtAddr,
    maxnode: usize,
) -> Result<u64, SystemError> {
    if nmask_ptr.is_null() || maxnode == 0 {
        return Ok(0);
    }

    let bytes_to_read = (maxnode + 7) / 8; // 将位转换为字节
    let bytes_to_read = bytes_to_read.min(8); // 限制为u64大小
    
    let reader = UserBufferReader::new(
        nmask_ptr.data() as *const u8,
        bytes_to_read,
        true,
    )?;
    
    let mut mask_bytes = [0u8; 8];
    reader.copy_from_user(&mut mask_bytes[..bytes_to_read], 0)?;
    
    Ok(u64::from_ne_bytes(mask_bytes))
}