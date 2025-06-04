use log::error;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_EXECVE;
use crate::filesystem::vfs::MAX_PATHLEN;
use crate::mm::page::PAGE_4K_SIZE;
use crate::mm::{verify_area, VirtAddr};
use crate::process::execve::do_execve;
use crate::process::{ProcessControlBlock, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{check_and_clone_cstr, check_and_clone_cstr_array};
use alloc::{ffi::CString, vec::Vec};
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
}

impl Syscall for SysExecve {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let path_ptr = Self::path_ptr(args);
        let argv_ptr = Self::argv_ptr(args);
        let env_ptr = Self::env_ptr(args);

        let virt_path_ptr = VirtAddr::new(path_ptr);
        let virt_argv_ptr = VirtAddr::new(argv_ptr);
        let virt_env_ptr = VirtAddr::new(env_ptr);

        // 权限校验
        if frame.is_from_user()
            && (verify_area(virt_path_ptr, MAX_PATHLEN).is_err()
                || verify_area(virt_argv_ptr, PAGE_4K_SIZE).is_err())
            || verify_area(virt_env_ptr, PAGE_4K_SIZE).is_err()
        {
            Err(SystemError::EFAULT)
        } else {
            let path = path_ptr as *const u8;
            let argv = argv_ptr as *const *const u8;
            let envp = env_ptr as *const *const u8;

            if path.is_null() {
                return Err(SystemError::EINVAL);
            }

            let x = || {
                let path: CString = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
                let argv: Vec<CString> = check_and_clone_cstr_array(argv)?;
                let envp: Vec<CString> = check_and_clone_cstr_array(envp)?;
                Ok((path, argv, envp))
            };
            let (path, argv, envp) = x().inspect_err(|e: &SystemError| {
                error!("Failed to execve: {:?}", e);
            })?;

            let path = path.into_string().map_err(|_| SystemError::EINVAL)?;
            ProcessManager::current_pcb()
                .basic_mut()
                .set_name(ProcessControlBlock::generate_name(&path, &argv));

            do_execve(path.clone(), argv, envp, frame)?;

            let pcb = ProcessManager::current_pcb();
            // 关闭设置了O_CLOEXEC的文件描述符
            let fd_table = pcb.fd_table();
            fd_table.write().close_on_exec();

            pcb.set_execute_path(path);

            return Ok(0);
        }
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
