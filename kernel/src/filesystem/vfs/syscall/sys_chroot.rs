//! System call handler for changing the root directory (chroot).

use system_error::SystemError;

use alloc::{string::String, vec::Vec};

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CHROOT;
use crate::filesystem::vfs::{FileType, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES};
use crate::process::cred::CAPFlags;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::check_and_clone_cstr;

/// System call handler for the `chroot` syscall
///
/// Changes the root directory of the calling process.
pub struct SysChrootHandle;

impl Syscall for SysChrootHandle {
	/// Returns the number of arguments expected by the `chroot` syscall
	fn num_args(&self) -> usize {
		1
	}

	/// 切换根目录（chroot）
	///
	/// 权限要求：CAP_SYS_CHROOT
	///
	/// 可能错误：
	/// - EFAULT: 用户指针无效
	/// - EINVAL: 路径编码非法
	/// - ENOENT: 目标不存在
	/// - ENOTDIR: 目标不是目录
	/// - EPERM: 缺少 CAP_SYS_CHROOT 权限
	fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
		let path_ptr = Self::path(args);

		if path_ptr.is_null() {
			return Err(SystemError::EFAULT);
		}

		// 权限检查：需要 CAP_SYS_CHROOT
		let pcb = ProcessManager::current_pcb();
		if !pcb.cred().has_capability(CAPFlags::CAP_SYS_CHROOT) {
			return Err(SystemError::EPERM);
		}

		let path = check_and_clone_cstr(path_ptr, Some(MAX_PATHLEN))?
			.into_string()
			.map_err(|_| SystemError::EINVAL)?;

		// 查找并跟随符号链接
		let root_inode = pcb.fs_struct().root();
		let inode = match root_inode.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)
		{
			Err(_) => return Err(SystemError::ENOENT),
			Ok(i) => i,
		};

		// 必须是目录
		let metadata = inode.metadata()?;
		if metadata.file_type != FileType::Dir {
			return Err(SystemError::ENOTDIR);
		}

		// 更新进程的 fs 视图：设置新的 root。
		// 同时将工作目录移动到新根，保证位于 chroot 内部。
		pcb.fs_struct_mut().set_root(inode);
		Ok(0)
	}

	/// Formats the syscall parameters for display/debug purposes
	fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
		vec![FormattedSyscallParam::new(
			"path",
			format!("{:#x}", Self::path(args) as usize),
		)]
	}
}

impl SysChrootHandle {
	/// Extracts the path argument from syscall parameters
	fn path(args: &[usize]) -> *const u8 {
		args[0] as *const u8
	}
}

syscall_table_macros::declare_syscall!(SYS_CHROOT, SysChrootHandle);

