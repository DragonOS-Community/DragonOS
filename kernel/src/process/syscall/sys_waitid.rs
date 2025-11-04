use core::mem::size_of;

use crate::arch::interrupt::TrapFrame;
use crate::ipc::signal_types::PosixSigInfo;
use crate::process::abi::WaitOption;
use crate::process::exit::kernel_waitid;
use crate::process::resource::RUsage;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use crate::{arch::syscall::nr::SYS_WAITID, ipc::syscall::sys_kill::PidConverter};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysWaitId;

impl SysWaitId {
    #[inline(always)]
    fn which(args: &[usize]) -> u32 {
        args[0] as u32
    }
    #[inline(always)]
    fn upid(args: &[usize]) -> i32 {
        args[1] as i32
    }
    #[inline(always)]
    fn infop(args: &[usize]) -> *mut PosixSigInfo {
        args[2] as *mut PosixSigInfo
    }
    #[inline(always)]
    fn options(args: &[usize]) -> usize {
        args[3]
    }
    #[inline(always)]
    fn rusage(args: &[usize]) -> *mut RUsage {
        args[4] as *mut RUsage
    }
}

impl Syscall for SysWaitId {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let which = Self::which(args);
        let upid = Self::upid(args);
        let infop_ptr = Self::infop(args);
        let options_bits = Self::options(args);
        let rusage_ptr = Self::rusage(args);
        // log::debug!(
        //     "sys_waitid: which={}, upid={}, infop={:#x}, options={:#x}, rusage={:#x}",
        //     which,
        //     upid,
        //     infop_ptr as usize,
        //     options_bits,
        //     rusage_ptr as usize
        // );

        let options = WaitOption::from_bits(options_bits as u32).ok_or(SystemError::EINVAL)?;
        // 至少包含一个事件位
        if !(options.contains(WaitOption::WEXITED)
            || options.contains(WaitOption::WSTOPPED)
            || options.contains(WaitOption::WCONTINUED))
        {
            return Err(SystemError::EINVAL);
        }

        // 构造 infop writer（可选）
        let infop_writer = if infop_ptr.is_null() {
            None
        } else {
            Some(UserBufferWriter::new(
                infop_ptr,
                size_of::<PosixSigInfo>(),
                true,
            )?)
        };

        // 构造 rusage 临时缓冲
        let mut tmp_rusage = if rusage_ptr.is_null() {
            None
        } else {
            Some(RUsage::default())
        };

        // which/upid → PidConverter（约定：P_ALL=0, P_PID=1, P_PGID=2, P_PIDFD=3）
        let pid_selector = match which {
            0..=2 => {
                match PidConverter::from_waitid(which, upid) {
                    Some(converter) => converter,
                    None => {
                        // 根据POSIX标准，当进程不存在或已被回收时，应该返回ECHILD
                        // 而不是ESRCH。这确保了与Linux行为的一致性。
                        return Err(SystemError::ECHILD);
                    }
                }
            }
            3 => {
                // P_PIDFD
                return Err(SystemError::ENOSYS);
            }
            _ => return Err(SystemError::EINVAL),
        };

        // 调用内核实现
        let _ = kernel_waitid(pid_selector, infop_writer, options, tmp_rusage.as_mut())?;
        // log::debug!("sys_waitid: kernel_waitid returned OK");

        if !rusage_ptr.is_null() {
            let mut rusage_writer =
                UserBufferWriter::new::<RUsage>(rusage_ptr, size_of::<RUsage>(), true)?;
            rusage_writer.copy_one_to_user(&tmp_rusage.unwrap(), 0)?;
        }

        // waitid 语义：成功返回 0
        Ok(0)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("which", format!("{:#x}", Self::which(args))),
            FormattedSyscallParam::new("upid", format!("{:#x}", Self::upid(args))),
            FormattedSyscallParam::new("infop", format!("{:#x}", Self::infop(args) as usize)),
            FormattedSyscallParam::new("options", format!("{:#x}", Self::options(args))),
            FormattedSyscallParam::new("rusage", format!("{:#x}", Self::rusage(args) as usize)),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_WAITID, SysWaitId);
