use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_EXECVE;
use crate::filesystem::vfs::{fcntl::AtFlags, MAX_PATHLEN};
use crate::process::exec_args::ExecArguments;
use crate::process::execve::{do_execve, ExecFailure};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::vfs_check_and_clone_cstr;
use alloc::{ffi::CString, string::String, vec::Vec};
use log::error;
use system_error::SystemError;

pub struct SysExecve;

impl SysExecve {
    fn path_ptr(args: &[usize]) -> usize {
        args[0]
    }

    fn argv_ptr(args: &[usize]) -> usize {
        args[1]
    }

    fn env_ptr(args: &[usize]) -> usize {
        args[2]
    }

    pub fn check_args(
        frame: &mut TrapFrame,
        path_ptr: usize,
        argv_ptr: usize,
        env_ptr: usize,
    ) -> Result<(), SystemError> {
        if path_ptr == 0 {
            return Err(SystemError::EINVAL);
        }
        // Each actual pathname/pointer/string read is checked by the protected
        // importer. A page-sized numerical precheck rejects short valid arrays
        // near the userspace boundary and does not establish readability.
        let _ = (frame, argv_ptr, env_ptr);
        Ok(())
    }

    pub(crate) fn basic_args(
        dirfd: i32,
        path: *const u8,
        argv: *const *const u8,
        envp: *const *const u8,
    ) -> Result<(CString, ExecArguments), SystemError> {
        let path = vfs_check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
        let name = crate::process::execve::exec_visible_name(
            dirfd,
            path.to_str().map_err(|_| SystemError::EINVAL)?,
        );
        let arguments =
            ExecArguments::from_user(&name, argv as usize, envp as usize)?.resolve_arg0()?;
        Ok((path, arguments))
    }

    pub(crate) fn execve(
        path: String,
        arguments: ExecArguments,
        frame: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        let result = do_execve(&path, arguments, frame);
        drop(path);
        result.map_err(ExecFailure::finish)
    }
}

impl Syscall for SysExecve {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path_ptr(args);
        let argv_ptr = Self::argv_ptr(args);
        let env_ptr = Self::env_ptr(args);

        Self::check_args(frame, path_ptr, argv_ptr, env_ptr)?;

        let (path, arguments) = Self::basic_args(
            AtFlags::AT_FDCWD.bits(),
            path_ptr as *const u8,
            argv_ptr as *const *const u8,
            env_ptr as *const *const u8,
        )
        .inspect_err(|e: &SystemError| {
            error!("Failed to execve: {:?}", e);
        })?;

        let path = path.into_string().map_err(|_| SystemError::EINVAL)?;

        // 如果路径为空字符串，返回 ENOENT
        if path.is_empty() {
            return Err(SystemError::ENOENT);
        }

        Self::execve(path, arguments, frame)?;
        return Ok(0);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("path", format!("{:#x}", Self::path_ptr(args))),
            FormattedSyscallParam::new("argv", format!("{:#x}", Self::argv_ptr(args))),
            FormattedSyscallParam::new("env", format!("{:#x}", Self::env_ptr(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_EXECVE, SysExecve);
