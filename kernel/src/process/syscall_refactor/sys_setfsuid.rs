use system_error::SystemError;
use alloc::vec::Vec;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::arch::syscall::nr::SYS_SETFSUID;
use crate::process::cred::Kuid;

pub struct SysSetFsuid;

impl SysSetFsuid{
    fn fsuid(args:&[usize])->usize{
        args[0]
    }
}

impl Syscall for SysSetFsuid{
    fn num_args(&self)->usize{
        1
    }

    fn handle(&self, args:&[usize],_from_user:bool)->Result<usize,SystemError>{
        let fsuid=Self::fsuid(args);
        let fsuid = Kuid::new(fsuid);

        let pcb = ProcessManager::current_pcb();
        let mut guard = pcb.cred.lock();
        let old_fsuid = guard.fsuid;

        if fsuid == guard.uid || fsuid == guard.euid || fsuid == guard.suid {
            guard.setfsuid(fsuid.data());
        }

        Ok(old_fsuid.data())
        
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new("fsuid", format!("{:#x}", Self::fsuid(args)))]
    }
}

syscall_table_macros::declare_syscall!(SYS_SETFSUID, SysSetFsuid);