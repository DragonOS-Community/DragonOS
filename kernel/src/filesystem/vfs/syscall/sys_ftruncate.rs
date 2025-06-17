use system_error::SystemError;

use crate::syscall::table::Syscall;
use crate::syscall::user_access::check_and_clone_cstr;

use super::{FileType, MAX_PATHLEN};
use crate::process::ProcessManager;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_FTRUNCATE;
use crate::syscall::table::FormattedSyscallParam;
use alloc::string::String;
use alloc::string::ToString;
use alloc::vec::Vec;

/// SYS_FTRUNCATE系统调用Handler
pub struct SysFtruncateHandle;

impl Syscall for SysFtruncateHandle {
    /// 返回参数个数
    fn num_args(&self) -> usize {
        2
    }
    // 处理函数
    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let fd = Self::fd(args);
        let len = Self::len(args);
        let res = do_ftruncate(path_ptr, len);
        res
    }
    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("fd", Self::fd(args).to_string()),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
        ]
    }
}

impl SysFtruncateHandle {
    /// Extracts the fd from syscall arguments
    fn fd(args: &[usize]) -> i32 {
        args[0] as i32
    }
    /// Extracts the length from syscall arguments
    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

syscall_table_macros::declare_syscall!(SYS_FTRUNCATE, SysFtruncateHandle);

/// # ftruncate
///
/// ## 描述
///
/// 改变文件大小.
/// 如果文件大小大于原来的大小，那么文件的内容将会被扩展到指定的大小，新的空间将会用0填充.
/// 如果文件大小小于原来的大小，那么文件的内容将会被截断到指定的大小.
///
/// ## 参数
///
/// - `fd`：文件描述符
/// - `len`：文件大小
///
/// ## 返回值
///
/// 如果成功，返回0，否则返回错误码.
pub fn do_ftruncate(fd: i32, len: usize) -> Result<usize, SystemError> {
    let binding = ProcessManager::current_pcb().fd_table();
    let fd_table_guard = binding.read();


    if let Some(file) = fd_table_guard.get_file_by_fd(fd) {
        // drop guard 以避免无法调度的问题
        drop(fd_table_guard);

	// 确保文件是普通文件而不是目录或其他文件类型
	let file_type = file.file_type(); // 获取文件类型
	// 不是普通文件返回-EINVAL
	if file_type != FileType::File {
            return Err(SystemError::EINVAL);
	}
	
        let r = file.ftruncate(len).map(|_| 0);
        return r;
    }

    return Err(SystemError::EBADF);
}


