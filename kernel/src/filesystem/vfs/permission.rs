//! VFS 权限检查基础设施
//!
//! 本模块为 DragonOS VFS 层提供 UNIX DAC（自主访问控制）权限检查功能，
//! 遵循 Linux 内核语义。

use super::Metadata;
use crate::{
    filesystem::vfs::{FileType, InodeMode},
    process::cred::{CAPFlags, Cred},
};
use system_error::SystemError;

bitflags! {
    pub struct PermissionMask: u32 {
        /// 测试执行权限
        const MAY_EXEC = 0x1;
        /// 测试写权限
        const MAY_WRITE = 0x2;
        /// 测试读权限
        const MAY_READ = 0x4;
        /// 测试追加权限
        const MAY_APPEND = 0x8;
        /// access() 系统调用使用
        const MAY_ACCESS = 0x10;
        /// 打开文件操作
        const MAY_OPEN = 0x20;
        /// 测试 chdir 操作（用于审计/LSM）
        const MAY_CHDIR = 0x40;

        const MAY_RWX = Self::MAY_READ.bits + Self::MAY_WRITE.bits + Self::MAY_EXEC.bits;
    }
}

impl Cred {
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
    pub fn inode_permission(&self, metadata: &Metadata, mask: u32) -> Result<(), SystemError> {
        // 从 mode 中提取权限位
        let file_mode = metadata.mode.bits();

        // 确定要检查哪组权限位
        let perm = if self.is_owner(metadata) {
            // 所有者权限（第 6-8 位）
            (file_mode & InodeMode::S_IRWXU.bits()) >> 6
        } else if self.in_group(metadata) {
            // 组权限（第 3-5 位）
            (file_mode & InodeMode::S_IRWXG.bits()) >> 3
        } else {
            // 其他用户权限（第 0-2 位）
            file_mode & InodeMode::S_IRWXO.bits()
        };

        // PermissionMask 的低 3 位已经是 Unix 权限位格式 (rwx)
        let need = mask & PermissionMask::MAY_RWX.bits();

        // 检查权限位是否满足请求
        if (need & !perm) == 0 {
            return Ok(()); // 通过普通检查，权限允许
        }

        // 尝试 capability 覆盖（类似 Linux 的 capable_wrt_inode_uidgid）
        if self.try_capability_override(metadata, mask) {
            return Ok(());
        }

        // 所有检查都失败
        Err(SystemError::EACCES)
    }

    /// 检查当前进程是否以 inode 所有者的身份运行
    #[inline]
    fn is_owner(&self, metadata: &Metadata) -> bool {
        self.fsuid.data() == metadata.uid
    }

    /// 检查进程是否在 inode 的所属组中
    fn in_group(&self, metadata: &Metadata) -> bool {
        // 检查主组
        if self.fsgid.data() == metadata.gid {
            return true;
        }
        // 检查附加组
        self.groups.iter().any(|gid| gid.data() == metadata.gid)
    }

    /// 尝试使用 capabilities 覆盖权限拒绝
    ///
    /// 实现 Linux 的 capable_wrt_inode_uidgid() 逻辑：
    /// - CAP_DAC_OVERRIDE: 绕过所有 DAC 检查
    /// - CAP_DAC_READ_SEARCH: 绕过读/搜索检查
    #[inline(never)]
    fn try_capability_override(&self, metadata: &Metadata, mask: u32) -> bool {
        // CAP_DAC_OVERRIDE: 绕过所有文件读、写和执行权限检查
        if self.has_capability(CAPFlags::CAP_DAC_OVERRIDE) {
            // Linux: CAP_DAC_OVERRIDE does not bypass execute checks for regular files
            // when no execute bit is set.
            if mask & PermissionMask::MAY_EXEC.bits() != 0
                && metadata.file_type != super::FileType::Dir
                && (metadata.mode.bits() & InodeMode::S_IXUGO.bits()) == 0
            {
                return false;
            }
            return true;
        }

        // CAP_DAC_READ_SEARCH: 绕过读和搜索（目录上的执行）检查
        if self.has_capability(CAPFlags::CAP_DAC_READ_SEARCH) {
            // 目录：只要不请求写权限，就允许 (即允许 Read 和 Exec/Search)
            if metadata.file_type == FileType::Dir {
                if (mask & PermissionMask::MAY_WRITE.bits()) == 0 {
                    return true;
                }
            } else {
                // 文件：仅允许只读权限
                let check_mask = mask
                    & (PermissionMask::MAY_READ.bits()
                        | PermissionMask::MAY_EXEC.bits()
                        | PermissionMask::MAY_WRITE.bits());
                if check_mask == PermissionMask::MAY_READ.bits() {
                    return true;
                }
            }
        }

        false
    }

    /// 检查 chdir 操作的权限
    ///
    /// ## 权限要求
    ///
    /// 目录的 chdir 操作需要执行(搜索)权限。
    ///
    /// ## MAY_CHDIR 标志说明
    ///
    /// `MAY_CHDIR` 标志主要用于语义标注和审计(audit)/LSM钩子,
    /// **不影响实际的 DAC (Discretionary Access Control) 权限检查**。
    /// 实际权限检查只依赖 `MAY_EXEC` 位。
    ///
    /// 这一设计与 Linux 内核保持一致,参见:
    /// - `fs/open.c::may_open()`
    /// - `security/security.c::security_path_chdir()`
    #[inline(never)]
    pub fn check_chdir_permission(&self, metadata: &Metadata) -> Result<(), SystemError> {
        // 验证是否为目录
        if metadata.file_type != FileType::Dir {
            return Err(SystemError::ENOTDIR);
        }

        // 检查执行权限(目录的搜索权限)
        // MAY_CHDIR 用于语义标注,不影响实际权限检查
        self.inode_permission(
            metadata,
            PermissionMask::MAY_EXEC.bits() | PermissionMask::MAY_CHDIR.bits(),
        )
    }
}
