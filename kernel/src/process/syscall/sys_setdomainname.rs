use alloc::string::ToString;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SETDOMAINNAME;
use crate::process::namespace::uts_namespace::NewUtsName;
use crate::process::ProcessManager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysSetdomainname;

impl SysSetdomainname {
    fn name(args: &[usize]) -> *mut u8 {
        args[0] as *mut u8
    }

    fn len(args: &[usize]) -> usize {
        args[1]
    }
}

impl Syscall for SysSetdomainname {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let name_ptr = Self::name(args);
        let len = Self::len(args);

        // 获取当前进程的 UTS namespace
        let uts_ns = ProcessManager::current_utsns();

        // 检查权限（需要 CAP_SYS_ADMIN）- 权限检查应该在长度验证之前
        if !uts_ns.check_uts_modify_permission() {
            return Err(SystemError::EPERM);
        }

        // 检查长度是否合法
        if len == 0 || len >= NewUtsName::MAXLEN {
            return Err(SystemError::EINVAL);
        }

        let reader = UserBufferReader::new_checked(name_ptr, len, true)?;
        let mut buf = vec![0u8; len];
        reader.copy_from_user_protected(&mut buf, 0)?;

        uts_ns.set_domainname(&buf)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("name", format!("{:#x}", Self::name(args) as usize)),
            FormattedSyscallParam::new("len", Self::len(args).to_string()),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETDOMAINNAME, SysSetdomainname);
