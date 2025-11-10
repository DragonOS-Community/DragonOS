use alloc::string::String;
#[allow(unused_imports)]
use alloc::string::ToString;
use alloc::sync::Arc;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_EXECVE;
use crate::filesystem::vfs::{IndexNode, MAX_PATHLEN, VFS_MAX_FOLLOW_SYMLINK_TIMES};
use crate::mm::page::PAGE_4K_SIZE;
use crate::mm::{verify_area, VirtAddr};
use crate::process::execve::do_execve;
use crate::process::{ProcessControlBlock, ProcessManager};
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::{check_and_clone_cstr, check_and_clone_cstr_array};
use alloc::{ffi::CString, vec::Vec};
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
        let virt_path_ptr = VirtAddr::new(path_ptr);
        let virt_argv_ptr = VirtAddr::new(argv_ptr);
        let virt_env_ptr = VirtAddr::new(env_ptr);

        // 权限校验
        if frame.is_from_user()
            && (verify_area(virt_path_ptr, MAX_PATHLEN).is_err()
                || verify_area(virt_argv_ptr, PAGE_4K_SIZE).is_err())
            || verify_area(virt_env_ptr, PAGE_4K_SIZE).is_err()
        {
            return Err(SystemError::EFAULT);
        }
        Ok(())
    }

    pub fn basic_args(
        path: *const u8,
        argv: *const *const u8,
        envp: *const *const u8,
    ) -> Result<(CString, Vec<CString>, Vec<CString>), SystemError> {
        let path: CString = check_and_clone_cstr(path, Some(MAX_PATHLEN))?;
        let mut argv: Vec<CString> = check_and_clone_cstr_array(argv)?;
        let envp: Vec<CString> = check_and_clone_cstr_array(envp)?;

        // 这里需要处理符号链接, 应用程序一般不支持嵌套符号链接
        // 如 test -> echo -> busybox, 需要内核代为解析到 echo, 传入 test 则不会让程序执行 echo 命令
        let root = ProcessManager::current_mntns().root_inode();
        if let Ok(real_inode) = root.lookup_follow_symlink2(
            argv[0].to_string_lossy().as_ref(),
            VFS_MAX_FOLLOW_SYMLINK_TIMES,
            false,
        ) {
            let real_path = real_inode.absolute_path()?;
            argv[0] = CString::new(real_path).unwrap();
        }

        Ok((path, argv, envp))
    }

    pub fn execve(
        inode: Arc<dyn IndexNode>,
        path: String,
        argv: Vec<CString>,
        envp: Vec<CString>,
        frame: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        ProcessManager::current_pcb()
            .basic_mut()
            .set_name(ProcessControlBlock::generate_name(&path));

        do_execve(inode, argv, envp, frame)?;

        let pcb = ProcessManager::current_pcb();
        // 关闭设置了O_CLOEXEC的文件描述符
        let fd_table = pcb.fd_table();
        fd_table.write().close_on_exec();

        pcb.set_execute_path(path);
        Ok(())
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

        let (path, argv, envp) = Self::basic_args(
            path_ptr as *const u8,
            argv_ptr as *const *const u8,
            env_ptr as *const *const u8,
        )
        .inspect_err(|e: &SystemError| {
            error!("Failed to execve: {:?}", e);
        })?;

        let path = path.into_string().map_err(|_| SystemError::EINVAL)?;

        let pwd = ProcessManager::current_pcb().pwd_inode();
        let inode = pwd.lookup_follow_symlink(&path, VFS_MAX_FOLLOW_SYMLINK_TIMES)?;

        Self::execve(inode, path, argv, envp, frame)?;
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
