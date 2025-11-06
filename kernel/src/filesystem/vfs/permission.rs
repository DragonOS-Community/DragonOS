//! VFS 权限检查基础设施
//!
//! 本模块为 DragonOS VFS 层提供 UNIX DAC（自主访问控制）权限检查功能，
//! 遵循 Linux 内核语义。

use super::Metadata;
use crate::process::cred::{CAPFlags, Cred};
use alloc::sync::Arc;
use system_error::SystemError;

bitflags! {
    pub struct PermissionMask: u32 {
        /// 测试执行权限
        const MAY_EXEC = 0x00000001;
        /// 测试写权限
        const MAY_WRITE = 0x00000002;
        /// 测试读权限
        const MAY_READ = 0x00000004;
        /// 测试追加权限
        const MAY_APPEND = 0x00000008;
        /// 测试 chdir 操作（用于审计/LSM）
        const MAY_CHDIR = 0x00000040;
    }
}

/// 检查具有给定凭证的进程是否有权限访问 inode。
///
/// 这是 DragonOS VFS 的核心权限检查函数，等价于 Linux 的
/// `inode_permission()` + `generic_permission()`。
///
/// ## 算法流程
///
/// 1. 检查所有者权限（mode >> 6）
/// 2. 如果在组内，检查组权限（mode >> 3）
/// 3. 检查其他用户权限（mode & 7）
/// 4. 如果被拒绝，尝试 capability 覆盖（CAP_DAC_OVERRIDE / CAP_DAC_READ_SEARCH）
///
/// ## 参数
///
/// - `metadata`: Inode 元数据（包含 mode、uid、gid）
/// - `cred`: 进程凭证（fsuid、fsgid、groups、capabilities）
/// - `mask`: 权限掩码（MAY_READ | MAY_WRITE | MAY_EXEC）
///
/// ## 返回值
///
/// - `Ok(())`: 权限允许
/// - `Err(SystemError::EACCES)`: 权限拒绝
///
/// ## 示例
///
/// ```rust
/// let metadata = inode.metadata()?;
/// let cred = ProcessManager::current_pcb().cred();
/// inode_permission(&metadata, &cred, MAY_EXEC)?;
/// ```
pub fn inode_permission(
    metadata: &Metadata,
    cred: &Arc<Cred>,
    mask: u32,
) -> Result<(), SystemError> {
    // 从 mode 中提取权限位
    let mode_bits = metadata.mode.bits();
    let file_mode = mode_bits & 0o777; // 只保留 rwxrwxrwx

    // 确定要检查哪组权限位
    let perm = if is_owner(metadata, cred) {
        // 所有者权限（第 6-8 位）
        (file_mode >> 6) & 7
    } else if in_group(metadata, cred) {
        // 组权限（第 3-5 位）
        (file_mode >> 3) & 7
    } else {
        // 其他用户权限（第 0-2 位）
        file_mode & 7
    };

    // PermissionMask 的低 3 位已经是 Unix 权限位格式 (rwx)
    let need = mask & 0b111;

    // 检查权限位是否满足请求
    if (need & !perm) == 0 {
        return Ok(()); // 通过普通检查，权限允许
    }

    // 尝试 capability 覆盖（类似 Linux 的 capable_wrt_inode_uidgid）
    if try_capability_override(metadata, cred, mask) {
        return Ok(());
    }

    // 所有检查都失败
    Err(SystemError::EACCES)
}

/// 检查当前进程是否以 inode 所有者的身份运行
#[inline]
fn is_owner(metadata: &Metadata, cred: &Arc<Cred>) -> bool {
    cred.fsuid.data() == metadata.uid
}

/// 检查进程是否在 inode 的所属组中
fn in_group(metadata: &Metadata, cred: &Arc<Cred>) -> bool {
    // 检查主组
    if cred.fsgid.data() == metadata.gid {
        return true;
    }

    // 检查附加组
    cred.groups.iter().any(|gid| gid.data() == metadata.gid)
}


/// 尝试使用 capabilities 覆盖权限拒绝
///
/// 实现 Linux 的 capable_wrt_inode_uidgid() 逻辑：
/// - CAP_DAC_OVERRIDE: 绕过所有 DAC 检查
/// - CAP_DAC_READ_SEARCH: 绕过读/搜索检查
fn try_capability_override(metadata: &Metadata, cred: &Arc<Cred>, mask: u32) -> bool {
    // CAP_DAC_OVERRIDE: 绕过所有文件读、写和执行权限检查
    if cred.has_capability(CAPFlags::CAP_DAC_OVERRIDE) {
        // 对于目录：总是允许
        log::debug!("CAP_DAC_OVERRIDE");
        if metadata.file_type == super::FileType::Dir {
            return true;
        }

        // 对于文件：如果不是仅执行请求，或文件对某人可执行，则允许
        if mask != PermissionMask::MAY_EXEC.bits() {
            return true;
        }
        if metadata.mode.bits() & 0o111 != 0 {
            return true;
        }
    }

    // CAP_DAC_READ_SEARCH: 绕过读和搜索（目录上的执行）检查
    if cred.has_capability(CAPFlags::CAP_DAC_READ_SEARCH) {
        // 允许读任何文件
        log::debug!("CAP_DAC_READ_SEARCH");
        if mask == PermissionMask::MAY_READ.bits() {
            return true;
        }

        // 允许搜索（执行）目录
        if metadata.file_type == super::FileType::Dir && mask == PermissionMask::MAY_EXEC.bits() {
            return true;
        }
    }

    false
}

/// 检查 chdir 操作的权限
pub fn check_chdir_permission(metadata: &Metadata, cred: &Arc<Cred>) -> Result<(), SystemError> {
    // 验证是否为目录
    if metadata.file_type != super::FileType::Dir {
        return Err(SystemError::ENOTDIR);
    }

    // 检查执行权限（目录的搜索权限）
    inode_permission(
        metadata,
        cred,
        PermissionMask::MAY_EXEC.bits() | PermissionMask::MAY_CHDIR.bits(),
    )
}
