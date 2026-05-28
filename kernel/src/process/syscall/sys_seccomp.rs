use alloc::{string::ToString, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_SECCOMP},
    process::seccomp,
    syscall::table::{FormattedSyscallParam, Syscall},
};

const SECCOMP_SET_MODE_STRICT: u32 = 0;
const SECCOMP_SET_MODE_FILTER: u32 = 1;
const SECCOMP_GET_ACTION_AVAIL: u32 = 2;

pub struct SysSeccomp;

impl Syscall for SysSeccomp {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let op = args[0] as u32;
        let flags = args[1] as u32;
        let uargs = args[2] as u64;

        match op {
            SECCOMP_SET_MODE_STRICT => {
                if flags != 0 || uargs != 0 {
                    return Err(SystemError::EINVAL);
                }
                seccomp::seccomp_set_mode_strict()?;
                Ok(0)
            }
            SECCOMP_SET_MODE_FILTER => {
                seccomp::seccomp_set_mode_filter(uargs, flags)?;
                Ok(0)
            }
            SECCOMP_GET_ACTION_AVAIL => {
                if flags != 0 {
                    return Err(SystemError::EINVAL);
                }
                seccomp::seccomp_get_action_avail(uargs)?;
                Ok(0)
            }
            _ => Err(SystemError::EINVAL),
        }
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        let op_val = args.first().copied().unwrap_or(0);
        let op_str = match op_val as u32 {
            SECCOMP_SET_MODE_STRICT => "SET_MODE_STRICT".to_string(),
            SECCOMP_SET_MODE_FILTER => "SET_MODE_FILTER".to_string(),
            SECCOMP_GET_ACTION_AVAIL => "GET_ACTION_AVAIL".to_string(),
            _ => format!("{:#x}", op_val),
        };

        vec![
            FormattedSyscallParam::new("op", op_str),
            FormattedSyscallParam::new(
                "flags",
                format!("{:#x}", args.get(1).copied().unwrap_or(0)),
            ),
            FormattedSyscallParam::new(
                "uargs",
                format!("{:#x}", args.get(2).copied().unwrap_or(0)),
            ),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SECCOMP, SysSeccomp);
