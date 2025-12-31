use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETGROUPS;
use crate::arch::syscall::nr::SYS_SETGROUPS;
use crate::process::cred::CAPFlags;
use crate::process::cred::Cred;
use crate::process::cred::Kgid;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferReader;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use core::mem::size_of;
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

        let mut tmp: Vec<u32> = Vec::with_capacity(cred.getgroups().len());
        for gid in cred.getgroups().iter() {
            tmp.push(gid.data() as u32);
        }

        // 使用 buffer_protected 方式进行基于异常表保护的拷贝
        let mut user_buffer =
            UserBufferWriter::new(args[1] as *mut u32, size * size_of::<u32>(), true)?;
        let mut buffer = user_buffer.buffer_protected(0)?;

        for (i, gid) in tmp.iter().enumerate() {
            buffer.write_one(i * size_of::<u32>(), gid)?;
        }
        Ok(tmp.len())
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

        // Linux: requires CAP_SETGID in the current user namespace.
        // For now we treat "root" or CAP_SETGID in effective set as privileged.
        let current_cred = pcb.cred();
        if current_cred.euid.data() != 0 && !current_cred.has_capability(CAPFlags::CAP_SETGID) {
            return Err(SystemError::EPERM);
        }

        let mut cred = (**pcb.cred.lock()).clone();
        let size = args[0];
        if size == 0 {
            // clear all supplementary groups
            cred.setgroups(Vec::new());
            return Ok(0);
        }
        if size > NGROUPS_MAX {
            return Err(SystemError::EINVAL);
        }

        // 使用 buffer_protected 方式进行基于异常表保护的拷贝
        let user_buffer =
            UserBufferReader::new(args[1] as *const u32, size * size_of::<u32>(), true)?;
        let buffer = user_buffer.buffer_protected(0)?;

        let mut raw_groups_bytes = vec![0u8; size * size_of::<u32>()];
        buffer.read_from_user(0, &mut raw_groups_bytes)?;

        let raw_groups: Vec<u32> = raw_groups_bytes
            .chunks_exact(size_of::<u32>())
            .map(|chunk| u32::from_ne_bytes(chunk.try_into().unwrap()))
            .collect();

        let groups: Vec<Kgid> = raw_groups
            .into_iter()
            .map(|g| Kgid::from(g as usize))
            .collect();
        cred.setgroups(groups);
        *pcb.cred.lock() = Cred::new_arc(cred);
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
