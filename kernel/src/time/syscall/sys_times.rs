use alloc::vec::Vec;

use system_error::SystemError;

use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_TIMES;
use crate::process::ProcessManager;
use crate::process::resource::RUsageWho;
use crate::sched::cputime::ns_to_clock_t;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use crate::time::syscall::posix_clock_now;
use crate::time::syscall::PosixClockID;

/// Linux `struct tms`（times(2) 的输出结构）
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct Tms {
    pub tms_utime: i64,
    pub tms_stime: i64,
    pub tms_cutime: i64,
    pub tms_cstime: i64,
}

pub struct SysTimes;

impl SysTimes {
    fn tms_ptr(args: &[usize]) -> *mut Tms {
        args[0] as *mut Tms
    }
}

impl Syscall for SysTimes {
    fn num_args(&self) -> usize {
        1
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let self_usage = pcb
            .get_rusage(RUsageWho::RUsageSelf)
            .ok_or(SystemError::EINVAL)?;
        let children_usage = pcb.get_rusage(RUsageWho::RUsageChildren);

        let tms = Tms {
            tms_utime: ns_to_clock_t(self_usage.ru_utime.to_ns()) as i64,
            tms_stime: ns_to_clock_t(self_usage.ru_stime.to_ns()) as i64,
            tms_cutime: children_usage
                .map(|u| ns_to_clock_t(u.ru_utime.to_ns()) as i64)
                .unwrap_or(0),
            tms_cstime: children_usage
                .map(|u| ns_to_clock_t(u.ru_stime.to_ns()) as i64)
                .unwrap_or(0),
        };
        // Linux 语义：返回自启动以来的时钟滴答数；缓冲区可为 NULL（仅返回滴答数）
        let now_ns = { let ts = posix_clock_now(PosixClockID::Monotonic)?; (ts.tv_sec as u64).saturating_mul(1_000_000_000).saturating_add(ts.tv_nsec as u64) };
        let ret = ns_to_clock_t(now_ns) as usize;

        let tms_ptr = Self::tms_ptr(args);
        if !tms_ptr.is_null() {
            let mut writer = UserBufferWriter::new::<Tms>(
                tms_ptr,
                core::mem::size_of::<Tms>(),
                true,
            )?;
            writer.buffer_protected(0)?.write_one(0, &tms)?;
        }
        Ok(ret)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![FormattedSyscallParam::new(
            "tms",
            format!("{:#x}", Self::tms_ptr(args) as usize),
        )]
    }
}

syscall_table_macros::declare_syscall!(SYS_TIMES, SysTimes);
