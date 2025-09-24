use alloc::string::ToString;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETHOSTNAME;
use crate::process::namespace::uts_namespace::NewUtsName;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::check_and_clone_cstr;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSethostname;

impl SysSethostname {
    fn name(args: &[usize]) -> *mut u8 {
        args[0] as *mut u8
    }

    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

impl Syscall for SysSethostname {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let name_ptr = Self::name(args);
        let len = Self::len(args);

        // 检查长度是否合法
        if len == 0 || len >= NewUtsName::MAXLEN {
            return Err(SystemError::EINVAL);
        }
        let s = check_and_clone_cstr(name_ptr, Some(NewUtsName::MAXLEN + 1))?;

        let ss = s.to_str().map_err(|_| SystemError::EINVAL)?;

        // 获取当前进程的 UTS namespace
        let uts_ns = ProcessManager::current_utsns();

        // 设置主机名
        uts_ns.set_hostname(ss)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETHOSTNAME, SysSethostname);
