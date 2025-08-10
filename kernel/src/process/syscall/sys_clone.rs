use crate::arch::interrupt::TrapFrame;
use crate::arch::syscall::nr::SYS_CLONE;
use crate::filesystem::procfs::procfs_register_pid;
use crate::mm::{VirtAddr, verify_area};
use crate::process::fork::{CloneFlags, KernelCloneArgs};
use crate::process::{KernelStack, ProcessControlBlock, ProcessManager};
use crate::sched::completion::Completion;
use crate::syscall::table::{FormattedSyscallParam, Syscall};
use crate::syscall::user_access::UserBufferWriter;
use alloc::vec::Vec;
use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

pub struct SysClone;

impl SysClone {
    fn parent_tid(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[2])
    }

    fn child_tid(args: &[usize]) -> VirtAddr {
        VirtAddr::new(args[3])
    }

    fn flags(args: &[usize]) -> CloneFlags {
        CloneFlags::from_bits_truncate(args[0] as u64)
    }

    fn stack(args: &[usize]) -> usize {
        args[1]
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

        let vfork = Arc::new(Completion::new());

        if flags.contains(CloneFlags::CLONE_PIDFD)
            && flags.contains(CloneFlags::CLONE_PARENT_SETTID)
        {
            return Err(SystemError::EINVAL);
        }

        let current_pcb = ProcessManager::current_pcb();
        let new_kstack = KernelStack::new()?;
        let name = current_pcb.basic().name().to_string();

        let pcb = ProcessControlBlock::new(name, new_kstack);
        // 克隆pcb
        ProcessManager::copy_process(&current_pcb, &pcb, clone_args, frame)?;

        // 向procfs注册进程
        procfs_register_pid(pcb.raw_pid()).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to register pid to procfs, pid: [{:?}]. Error: {:?}",
                pcb.raw_pid(),
                e
            )
        });

        if flags.contains(CloneFlags::CLONE_VFORK) {
            pcb.thread.write_irqsave().vfork_done = Some(vfork.clone());
        }

        if pcb.thread.read_irqsave().set_child_tid.is_some() {
            let addr = pcb.thread.read_irqsave().set_child_tid.unwrap();
            let mut writer =
                UserBufferWriter::new(addr.as_ptr::<i32>(), core::mem::size_of::<i32>(), true)?;
            writer.copy_one_to_user(&(pcb.raw_pid().data() as i32), 0)?;
        }

        ProcessManager::wakeup(&pcb).unwrap_or_else(|e| {
            panic!(
                "fork: Failed to wakeup new process, pid: [{:?}]. Error: {:?}",
                pcb.raw_pid(),
                e
            )
        });

        if flags.contains(CloneFlags::CLONE_VFORK) {
            // 等待子进程结束或者exec;
            vfork.wait_for_completion_interruptible()?;
        }

        return Ok(pcb.raw_pid().0);
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
