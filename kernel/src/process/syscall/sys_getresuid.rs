use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_GETRESUID;
use crate::process::ProcessManager;
use crate::syscall::table::FormattedSyscallParam;
use crate::syscall::table::Syscall;
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysGetResUid;

impl SysGetResUid {
    fn ruidp(args: &[usize]) -> *mut u32 {
        args[0] as *mut u32
    }

    fn euidp(args: &[usize]) -> *mut u32 {
        args[1] as *mut u32
    }

    fn suidp(args: &[usize]) -> *mut u32 {
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

impl Syscall for SysGetResUid {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let cred = pcb.cred.lock();

        let ruid: u32 = cred.uid.data() as u32;
        let euid: u32 = cred.euid.data() as u32;
        let suid: u32 = cred.suid.data() as u32;

        Self::write_id_protected(Self::ruidp(args), ruid)?;
        Self::write_id_protected(Self::euidp(args), euid)?;
        Self::write_id_protected(Self::suidp(args), suid)?;

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("ruid", format!("{:#x}", Self::ruidp(args) as usize)),
            FormattedSyscallParam::new("euid", format!("{:#x}", Self::euidp(args) as usize)),
            FormattedSyscallParam::new("suid", format!("{:#x}", Self::suidp(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_GETRESUID, SysGetResUid);
