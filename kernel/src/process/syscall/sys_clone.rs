use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLONE;
use crate::mm::{verify_area, VirtAddr};
use crate::process::fork::{CloneFlags, KernelCloneArgs};
use crate::process::syscall::clone_utils::do_clone;
use crate::process::Signal;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use alloc::vec::Vec;
use system_error::SystemError;

pub struct SysClone;

impl SysClone {
    fn flags(args: &[usize]) -> CloneFlags {
        CloneFlags::from_bits_truncate(args[0] as u64)
    }

    fn stack(args: &[usize]) -> usize {
        args[1]
    }
    fn parent_tid(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[2])
    }

    fn child_tid(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[3])
    }

    fn tls(args: &[usize]) -> usize {
        args[4]
    }
}

impl Syscall for SysClone {
    fn num_args(&self) -> usize {
        5
    }

    fn handle(&self, args: &[usize], frame: &mut TrapFrame) -> Result<usize, SystemError> {
        let parent_tid = Self::parent_tid(args);
        let child_tid = Self::child_tid(args);

        // 地址校验
        verify_area(parent_tid, core::mem::size_of::<i32>())?;
        verify_area(child_tid, core::mem::size_of::<i32>())?;

        let flags = Self::flags(args);
        let stack = Self::stack(args);
        let tls = Self::tls(args);

        let mut clone_args = KernelCloneArgs::new();
        clone_args.flags = flags;
        clone_args.stack = stack;
        clone_args.parent_tid = parent_tid;
        clone_args.child_tid = child_tid;
        clone_args.tls = tls;

        // 旧版 clone() 系统调用中，flags 的低 8 位用于指定 exit_signal
        let exit_signal_num = (args[0] & 0xFF) as i32;
        clone_args.exit_signal = Signal::from(exit_signal_num);

        do_clone(clone_args, frame)
    }

    fn entry_format(&self, args: &[usize]) -> Vec<FormattedSyscallParam> {
        vec![
            FormattedSyscallParam::new(
                "parent_tid",
                format!("{:#x}", Self::parent_tid(args).data()),
            ),
            FormattedSyscallParam::new("child_tid", format!("{:#x}", Self::child_tid(args).data())),
            FormattedSyscallParam::new("flags", format!("{:#x}", Self::flags(args))),
            FormattedSyscallParam::new("stack", format!("{:#x}", Self::stack(args))),
            FormattedSyscallParam::new("tls", format!("{:#x}", Self::tls(args))),
        ]
    }
}

syscall_table_macros::declare_syscall!(SYS_CLONE, SysClone);
