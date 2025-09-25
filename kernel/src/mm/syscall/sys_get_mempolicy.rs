//! System call handler for the get_mempolicy system call.

use crate::arch::{interrupt::TrapFrame, syscall::nr::SYS_GET_MEMPOLICY};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::mm::{
    VirtAddr, ucontext::AddressSpace,
};
use alloc::vec::Vec;
use system_error::SystemError;

use super::mempolice_utils::{
    Mempolicy, MempolicyFlags,
    write_policy_to_user, write_nodemask_to_user
};

/// ## get_mempolicy系统调用
///
/// 获取进程的NUMA内存策略
///
/// ## 参数
///
/// - `policy`: 用于返回内存策略模式的用户空间指针
/// - `nmask`: 用于返回节点掩码的用户空间指针  
/// - `maxnode`: nmask指向的位图中的最大节点数
/// - `addr`: 要查询策略的内存地址（当flags包含MPOL_F_ADDR时使用）
/// - `flags`: 控制行为的标志位
///
/// ## 返回值
///
/// 成功时返回0，失败时返回错误码 
pub struct SysGetMempolicy;

impl Syscall for SysGetMempolicy {
    /// Returns the number of arguments this syscall takes.
    fn num_args(&self) -> usize {
        5
    }

    /// Handles the get_mempolicy system call.
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let policy_ptr = VirtAddr::new(Self::policy(args));
        let nmask_ptr = VirtAddr::new(Self::nmask(args));
        let maxnode = Self::maxnode(args);
        let addr = VirtAddr::new(Self::addr(args));
        let flags = Self::flags(args) as u32;

        if flags &
		    !((MempolicyFlags::MPOL_F_NODE.bits())
			| (MempolicyFlags::MPOL_F_ADDR.bits())
			| (MempolicyFlags::MPOL_F_MEMS_ALLOWED.bits())) > 0
		{ return Err(SystemError::EINVAL); }

        if flags & MempolicyFlags::MPOL_F_MEMS_ALLOWED.bits() > 0 {
            if (flags & (MempolicyFlags::MPOL_F_NODE.bits() | MempolicyFlags::MPOL_F_ADDR.bits())) > 0 {
                return Err(SystemError::EINVAL);
            }
        }
        
        // 验证参数
        if maxnode > 1024 {
            return Err(SystemError::EINVAL);
        }

        // 获取当前进程的内存策略
        let mempolicy = Self::get_process_mempolicy(addr, flags)?;

        // 根据flags决定返回的内容
        if flags & MempolicyFlags::MPOL_F_NODE.bits() > 0 {
            // 返回首选节点或有效节点
            let node = mempolicy.preferred_node.unwrap_or(0);
            write_policy_to_user(policy_ptr, node)?;
        } else if flags & MempolicyFlags::MPOL_F_MEMS_ALLOWED.bits() > 0 {
            // 返回允许的内存节点掩码
            let allowed_mask = Self::get_allowed_nodes()?;
            write_nodemask_to_user(nmask_ptr, allowed_mask, maxnode)?;
        } else {
            // 返回策略模式
            write_policy_to_user(policy_ptr, mempolicy.mode_as_u32())?;
            
            // 返回节点掩码
            if mempolicy.is_node_policy() {
                write_nodemask_to_user(nmask_ptr, mempolicy.nodemask, maxnode)?;
            }
        }

        Ok(0)
    }

    /// Formats the syscall arguments for display/debugging purposes.
    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("policy", format!("{:#x}", Self::policy(args))),
            FormattedSyscallParam::new("nmask", format!("{:#x}", Self::nmask(args))),
            FormattedSyscallParam::new("maxnode", format!("{}", Self::maxnode(args))),
            FormattedSyscallParam::new("addr", format!("{:#x}", Self::addr(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

impl SysGetMempolicy {
    fn policy(args: &[usize]) -> usize {
        args[0]
    }
    fn nmask(args: &[usize]) -> usize {
        args[1]
    }
    fn maxnode(args: &[usize]) -> usize {
        args[2]
    }
    fn addr(args: &[usize]) -> usize {
        args[3]
    }
    fn flags(args: &[usize]) -> usize {
        args[4]
    }

    /// 获取进程的内存策略
    fn get_process_mempolicy(
        addr: VirtAddr, 
        flags: u32
    ) -> Result<Mempolicy, SystemError> {
        if flags & MempolicyFlags::MPOL_F_ADDR.bits() > 0 {
            // 查询特定地址的策略
            Self::get_vma_mempolicy(addr)
        } else {
            // 查询进程默认策略
            Self::get_default_mempolicy()
        }
    }

    /// 获取VMA的内存策略
    fn get_vma_mempolicy(addr: VirtAddr) -> Result<Mempolicy, SystemError> {
        let current_as = AddressSpace::current()?;
        let as_guard = current_as.read();
        
        // 检查地址是否在有效的VMA中
        if let Some(_vma) = as_guard.mappings.contains(addr) {
            // 目前DragonOS还没有实现per-VMA的内存策略
            // 返回默认策略
            Ok(Mempolicy::new_default())
        } else {
            Err(SystemError::EFAULT)
        }
    }

    /// 获取默认内存策略
    fn get_default_mempolicy() -> Result<Mempolicy, SystemError> {
        // 目前DragonOS是单节点系统，返回默认策略
        Ok(Mempolicy::new_default())
    }

    /// 获取允许的NUMA节点掩码
    fn get_allowed_nodes() -> Result<u64, SystemError> {
        // 目前DragonOS是单节点系统，只有节点0可用
        Ok(1u64) // 只有第0位设置，表示只有节点0
    }
}

syscall_table_macros::declare_syscall!(SYS_GET_MEMPOLICY, SysGetMempolicy);