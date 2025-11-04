use super::kexec_core::do_kexec_load;
use super::KexecSegment;
use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_KEXEC_LOAD;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferReader;
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysKexecLoad;

impl SysKexecLoad {
    fn entry(args: &[usize]) -> usize {
        args[0]
    }

    fn nr_segments(args: &[usize]) -> usize {
        args[1]
    }

    fn segments_ptr(args: &[usize]) -> usize {
        args[2]
    }

    fn flags(args: &[usize]) -> usize {
        args[3]
    }
}

impl Syscall for SysKexecLoad {
    fn num_args(&self) -> usize {
        4
    }

    fn handle(&self, args: &[usize], _frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let entry = Self::entry(args);
        let nr_segments = Self::nr_segments(args);
        let segments_ptr = Self::segments_ptr(args);
        let flags = Self::flags(args);

        // TODO: do some check

        let usegments_buf = UserBufferReader::new::<KexecSegment>(
            segments_ptr as *mut KexecSegment,
            core::mem::size_of::<KexecSegment>() * nr_segments,
            true,
        )?;
        let ksegments: &[KexecSegment] = usegments_buf.read_from_user(0)?;

        do_kexec_load(entry, nr_segments, ksegments, flags)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new("entry", format!("{:#x}", Self::entry(args))),
            FormattedSyscallParam::new("nr_segments", format!("{:#x}", Self::nr_segments(args))),
            FormattedSyscallParam::new("segments_ptr", format!("{:#x}", Self::segments_ptr(args))),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_KEXEC_LOAD, SysKexecLoad);
