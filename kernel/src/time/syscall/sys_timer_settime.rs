use crate::{
    arch::{interrupt::TrapFrame, syscall::nr::SYS_TIMER_SETTIME},
    process::posix_timer::PosixItimerspec,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::{UserBufferReader, UserBufferWriter},
    },
};
use alloc::vec::Vec;
use core::mem::size_of;
use syscall_table_macros::declare_syscall;
use system_error::SystemError;

pub struct SysTimerSettimeHandle;

impl SysTimerSettimeHandle {
    fn timerid(args: &[usize]) -> i32 {
        args[0] as i32
    }
    fn flags(args: &[usize]) -> i32 {
        args[1] as i32
    }
    fn new_value(args: &[usize]) -> *const PosixItimerspec {
        args[2] as *const PosixItimerspec
    }
    fn old_value(args: &[usize]) -> *mut PosixItimerspec {
        args[3] as *mut PosixItimerspec
    }
}

impl Syscall for SysTimerSettimeHandle {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let timerid = Self::timerid(args);
        let flags = Self::flags(args);
        let new_value_ptr = Self::new_value(args);
        let old_value_ptr = Self::old_value(args);

        // 暂不支持 TIMER_ABSTIME 等 flag
        if flags != 0 {
            return Err(SystemError::EINVAL);
        }
        if new_value_ptr.is_null() {
            return Err(SystemError::EINVAL);
        }

        let reader = UserBufferReader::new(new_value_ptr, size_of::<PosixItimerspec>(), true)?;
        // 用异常表保护版本读取，避免用户地址缺页/无效导致内核崩溃
        let new_value = reader.buffer_protected(0)?.read_one::<PosixItimerspec>(0)?;

        let pcb = ProcessManager::current_pcb();
        let old = pcb
            .posix_timers_irqsave()
            .settime(&pcb, timerid, new_value)?;

        if !old_value_ptr.is_null() {
            let mut writer =
                UserBufferWriter::new(old_value_ptr, size_of::<PosixItimerspec>(), true)?;
            // 用异常表保护版本写回
            writer
                .buffer_protected(0)?
                .write_one::<PosixItimerspec>(0, &old)?;
        }

        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("timerid", format!("{}", args[0])),
            FormattedSyscallParam::new("flags", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("new_value", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("old_value", format!("{:#x}", args[3])),
        ]
    }
}

declare_syscall!(SYS_TIMER_SETTIME, SysTimerSettimeHandle);
