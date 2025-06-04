use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_WAIT4;
use crate::process::abi::WaitOption;
use crate::process::exit::kernel_wait4;
use crate::process::resource::RUsage;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use core::ffi::c_int;
use core::ffi::c_void;
use system_error::SystemError;

pub struct SysWait4;

impl SysWait4 {
    fn pid(args: &[usize]) -> i32 {
        args[0] as i32
    }

    fn wstatus(args: &[usize]) -> *mut i32 {
        args[1] as *mut i32
    }

    fn options(args: &[usize]) -> c_int {
        args[2] as c_int
    }

    fn rusage(args: &[usize]) -> *mut c_void {
        args[3] as *mut c_void
    }
}

impl Syscall for SysWait4 {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = Self::pid(args);
        let wstatus = Self::wstatus(args);
        let options = Self::options(args);
        let rusage = Self::rusage(args);
        // 权限校验
        // todo: 引入rusage之后，更正以下权限校验代码中，rusage的大小

        let options = WaitOption::from_bits(options as u32).ok_or(SystemError::EINVAL)?;

        let wstatus_buf = if wstatus.is_null() {
            None
        } else {
            Some(UserBufferWriter::new(
                wstatus,
                core::mem::size_of::<i32>(),
                true,
            )?)
        };

        let mut tmp_rusage = if rusage.is_null() {
            None
        } else {
            Some(RUsage::default())
        };

        let r = kernel_wait4(pid, wstatus_buf, options, tmp_rusage.as_mut())?;

        if !rusage.is_null() {
            let mut rusage_buf = UserBufferWriter::new::<RUsage>(
                rusage as *mut RUsage,
                core::mem::size_of::<RUsage>(),
                true,
            )?;
            rusage_buf.copy_one_to_user(&tmp_rusage.unwrap(), 0)?;
        }
        return Ok(r);
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", format!("{:#x}", Self::pid(args))),
            FormattedSyscallParam::new("wstatus", format!("{:#x}", Self::wstatus(args) as usize)),
            FormattedSyscallParam::new("options", format!("{:#x}", Self::options(args))),
            FormattedSyscallParam::new("rusage", format!("{:#x}", Self::rusage(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_WAIT4, SysWait4);
