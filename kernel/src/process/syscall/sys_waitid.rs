use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_WAITID;
use crate::process::abi::WaitOption;
use crate::process::exit::kernel_waitid;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use alloc::vec::Vec;
use core::ffi::c_int;
use system_error::SystemError;

pub struct SysWaitId;

impl SysWaitId {
    #[inline(always)]
    fn idtype(args: &[usize]) -> i32 {
        args[0] as i32
    }

    #[inline(always)]
    fn id(args: &[usize]) -> i32 {
        args[1] as i32
    }

    #[inline(always)]
    fn siginfo(args: &[usize]) -> *mut i32 {
        args[2] as *mut i32
    }

    #[inline(always)]
    fn options(args: &[usize]) -> c_int {
        args[3] as c_int
    }
}

impl Syscall for SysWaitId {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let idtype = Self::idtype(args);
        let id = Self::id(args);
        let siginfo = Self::siginfo(args);
        let options = Self::options(args);

        //log::info!("waitid, which:{}, tgid:{}", idtype, id);

        let options = WaitOption::from_bits(options as u32).ok_or(SystemError::EINVAL)?;

        let r = kernel_waitid(idtype, id, siginfo, options, None)?;

        //log::info!("waitid done, r:{}", r);

        Ok(r)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("idtype", format!("{:#x}", Self::idtype(args))),
            FormattedSyscallParam::new("id", format!("{:#x}", Self::id(args))),
            FormattedSyscallParam::new("siginfo", format!("{:#x}", Self::siginfo(args) as usize)),
            FormattedSyscallParam::new("options", format!("{:#x}", Self::options(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_WAITID, SysWaitId);
