use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETGROUPS;
use crate::arch::syscall::nr::SYS_SETGROUPS;
use crate::process::cred::Kgid;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

const NGROUPS_MAX: usize = 65536;

/// See https://man7.org/linux/man-pages/man2/setgroups.2.html
pub struct SysGetGroups;

impl Syscall for SysGetGroups {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred.lock();
        let size = args[0];
        if size == 0 {
            return Ok(cred.getgroups().len());
        }
        if size < cred.getgroups().len() || size > NGROUPS_MAX {
            return Err(SystemError::EINVAL);
        }
        let mut user_buffer = UserBufferWriter::new(
            args[1] as *mut Kgid,
            size * core::mem::size_of::<Kgid>(),
            true,
        )?;
        user_buffer.copy_to_user(cred.getgroups(), 0)?;
        Ok(size)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("size", format!("{}", args[0])),
            FormattedSyscallParam::new("list", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETGROUPS, SysGetGroups);

pub struct SysSetGroups;

impl Syscall for SysSetGroups {
    fn num_args(&self) -> usize {
        2
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let mut cred = pcb.cred.lock();
        let size = args[0];
        if size == 0 {
            // clear all supplementary groups
            cred.setgroups(Vec::new());
            return Ok(0);
        }
        if size > NGROUPS_MAX {
            return Err(SystemError::EINVAL);
        }
        let user_buffer = UserBufferReader::new(
            args[1] as *const Kgid,
            size * core::mem::size_of::<Kgid>(),
            true,
        )?;
        let mut groups = vec![Kgid::from(0); size];
        user_buffer.copy_from_user(&mut groups, 0).unwrap();
        // log::info!("set supplementary groups: {:?}", groups);
        cred.setgroups(groups);
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("size", format!("{}", args[0])),
            FormattedSyscallParam::new("list", format!("{:#x}", args[1])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETGROUPS, SysSetGroups);
