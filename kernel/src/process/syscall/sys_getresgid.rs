use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETRESGID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetResGid;

impl SysGetResGid {
    fn rgidp(args: &[usize]) -> *mut u32 {
        args[0] as *mut u32
    }

    fn egidp(args: &[usize]) -> *mut u32 {
        args[1] as *mut u32
    }

    fn sgidp(args: &[usize]) -> *mut u32 {
        args[2] as *mut u32
    }

    /// 使用异常表保护的方式向用户空间写入单个 u32 值
    fn write_id_protected(ptr: *mut u32, value: u32) -> Result<(), SystemError> {
        if ptr.is_null() {
            return Ok(());
        }
        let mut writer = UserBufferWriter::new(ptr, core::mem::size_of::<u32>(), true)?;
        let mut buffer = writer.buffer_protected(0)?;
        buffer.write_one(0, &value)
    }
}

impl Syscall for SysGetResGid {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred.lock();

        let rgid: u32 = cred.gid.data() as u32;
        let egid: u32 = cred.egid.data() as u32;
        let sgid: u32 = cred.sgid.data() as u32;

        Self::write_id_protected(Self::rgidp(args), rgid)?;
        Self::write_id_protected(Self::egidp(args), egid)?;
        Self::write_id_protected(Self::sgidp(args), sgid)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("rgid", format!("{:#x}", Self::rgidp(args) as usize)),
            FormattedSyscallParam::new("egid", format!("{:#x}", Self::egidp(args) as usize)),
            FormattedSyscallParam::new("sgid", format!("{:#x}", Self::sgidp(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETRESGID, SysGetResGid);
