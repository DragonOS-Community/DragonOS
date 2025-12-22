use alloc::vec::Vec;
use core::mem::size_of;

use system_error::SystemError;

use crate::{
    arch::{ipc::signal::SigSet, syscall::nr::SYS_PSELECT6},
    filesystem::{
        poll::{poll_select_finish, poll_select_set_timeout, PollTimeType},
        vfs::syscall::sys_select::{do_sys_select, FdSet},
    },
    ipc::signal::set_user_sigmask,
    process::ProcessManager,
    syscall::{
        table::{FormattedSyscallParam, Syscall},
        user_access::UserBufferReader,
    },
    time::{Instant, PosixTimeSpec, NSEC_PER_SEC},
};

/// MaskWithSize结构，用于pselect6系统调用
/// 参考Linux内核: include/linux/syscalls.h
#[repr(C)]
#[derive(Clone, Copy)]
struct MaskWithSize {
    mask: usize,      // sigset_t* 指针
    mask_size: usize, // size_t
}

pub struct SysPselect6;
impl Syscall for SysPselect6 {
    fn num_args(&self) -> usize {
        6
    }

    fn handle(
        &self,
        args: &[usize],
        _frame: &mut crate::arch::interrupt::TrapFrame,
    ) -> Result<usize, SystemError> {
        let nfds = args[0];
        let readfds_addr = args[1];
        let writefds_addr = args[2];
        let exceptfds_addr = args[3];
        let timeout_ptr = args[4];
        let sigmask_ptr = args[5];

        // 处理sigmask参数（MaskWithSize结构）
        let mut sigmask: Option<SigSet> = None;
        if sigmask_ptr != 0 {
            let mask_with_size_reader = UserBufferReader::new(
                sigmask_ptr as *const MaskWithSize,
                size_of::<MaskWithSize>(),
                true,
            )?;
            let mask_with_size = mask_with_size_reader
                .buffer_protected(0)?
                .read_one::<MaskWithSize>(0)?;

            // 验证mask_size
            // 如果mask为nullptr，size可以是任意值（EmptySigMask测试）
            // 如果mask不为nullptr，size必须是8的倍数且>=8
            if mask_with_size.mask != 0 {
                if mask_with_size.mask_size < 8 || (mask_with_size.mask_size % 8) != 0 {
                    return Err(SystemError::EINVAL);
                }
                let sigmask_reader = UserBufferReader::new(
                    mask_with_size.mask as *const SigSet,
                    size_of::<SigSet>(),
                    true,
                )?;
                sigmask = Some(sigmask_reader.buffer_protected(0)?.read_one::<SigSet>(0)?);
            }
        }

        // 处理timeout参数（timespec）
        let mut end_time: Option<Instant> = None;
        if timeout_ptr != 0 {
            let tsreader = UserBufferReader::new(
                timeout_ptr as *const PosixTimeSpec,
                size_of::<PosixTimeSpec>(),
                true,
            )?;
            let ts = tsreader.buffer_protected(0)?.read_one::<PosixTimeSpec>(0)?;

            // 验证timeout参数
            // 1. 检查是否为负值
            if ts.tv_sec < 0 || ts.tv_nsec < 0 {
                return Err(SystemError::EINVAL);
            }
            // 2. 检查tv_nsec是否在有效范围内 [0, NSEC_PER_SEC - 1]
            // 如果tv_nsec >= NSEC_PER_SEC，说明timeout未规范化，应该返回EINVAL
            if ts.tv_nsec >= NSEC_PER_SEC as i64 {
                return Err(SystemError::EINVAL);
            }

            // 计算超时时间（毫秒）
            let timeout_ms = ts.as_millis();
            if timeout_ms >= 0 {
                end_time = poll_select_set_timeout(timeout_ms as u64);
            }
        }

        // 设置信号掩码
        if let Some(mut sigmask) = sigmask {
            set_user_sigmask(&mut sigmask);
            // 重新计算pending信号状态，因为信号掩码改变了
            // 这样has_pending_not_masked_signal才能正确工作
            ProcessManager::current_pcb().recalc_sigpending(None);
        }

        // 执行select操作
        let result = do_sys_select(
            nfds as isize,
            readfds_addr as *const FdSet,
            writefds_addr as *const FdSet,
            exceptfds_addr as *const FdSet,
            end_time,
        );

        // 更新用户空间的timeout为剩余时间（使用TimeSpec类型）
        poll_select_finish(end_time, timeout_ptr, PollTimeType::TimeSpec, result)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<crate::syscall::table::FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("nfds", format!("{}", args[0])),
            FormattedSyscallParam::new("readfds", format!("{:#x}", args[1])),
            FormattedSyscallParam::new("writefds", format!("{:#x}", args[2])),
            FormattedSyscallParam::new("exceptfds", format!("{:#x}", args[3])),
            FormattedSyscallParam::new("timeout", format!("{:#x}", args[4])),
            FormattedSyscallParam::new("sigmask", format!("{:#x}", args[5])),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_PSELECT6, SysPselect6);
