use alloc::string::ToString;
use alloc::vec::Vec;
use bitmap::traits::BitMapOps;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_GETAFFINITY;
use crate::process::{ProcessManager, RawPid};
use crate::sched::syscall::util::has_sched_permission;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::copy_to_user_protected;

pub struct SysSchedGetaffinity;

impl Syscall for SysSchedGetaffinity {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pid = args[0] as i32;
        let size = args[1];
        let set_vaddr = args[2];

        if size == 0 {
            return Err(SystemError::EINVAL);
        }

        let target_pcb = if pid == 0 {
            ProcessManager::current_pcb()
        } else {
            ProcessManager::find_task_by_vpid(RawPid::from(pid as usize))
                .ok_or(SystemError::ESRCH)?
        };

        let current_pcb = ProcessManager::current_pcb();
        if !has_sched_permission(&current_pcb, &target_pcb) {
            return Err(SystemError::EPERM);
        }

        let mask = target_pcb.sched_info().cpus_allowed();
        let src = unsafe { mask.inner().as_bytes() };
        let copy_len = core::cmp::min(size, src.len());

        unsafe { copy_to_user_protected(crate::mm::VirtAddr::new(set_vaddr), &src[..copy_len])? };

        Ok(copy_len)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", (args[0] as i32).to_string()),
            FormattedSyscallParam::new("size", args[1].to_string()),
            FormattedSyscallParam::new("set", format!("0x{:x}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GETAFFINITY, SysSchedGetaffinity);
