use alloc::string::ToString;
use alloc::vec::Vec;
use bitmap::traits::BitMapOps;
use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_SCHED_GETAFFINITY;
use crate::smp::cpu::smp_cpu_manager;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;

pub struct SysSchedGetaffinity;

impl Syscall for SysSchedGetaffinity {
    fn num_args(&self) -> usize {
        3
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let _pid = args[0] as i32;
        let size = args[1];
        let set_vaddr = args[2];

        // 验证用户空间地址
        let mut user_buffer_writer =
            UserBufferWriter::new(set_vaddr as *mut u8, size, frame.is_from_user())?;
        let set: &mut [u8] = user_buffer_writer.buffer(0)?;

        // 获取CPU亲和性掩码
        let cpu_manager = smp_cpu_manager();
        let src = unsafe { cpu_manager.possible_cpus().inner().as_bytes() };

        // 确保不会越界
        let copy_len = core::cmp::min(size, src.len());
        set[..copy_len].copy_from_slice(&src[..copy_len]);

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("pid", args[0].to_string()),
            FormattedSyscallParam::new("size", args[1].to_string()),
            FormattedSyscallParam::new("set", format!("0x{:x}", args[2])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_SCHED_GETAFFINITY, SysSchedGetaffinity);
