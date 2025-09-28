use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::MAX_SIG_NUM;
use crate::filesystem::procfs::procfs_register_pid;
use crate::process::fork::{CloneFlags, KernelCloneArgs, MAX_PID_NS_LEVEL};
use crate::process::{KernelStack, ProcessControlBlock, ProcessManager};
use crate::sched::completion::Completion;
use crate::syscall::user_access::UserBufferWriter;
use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

pub const CLONE_ARGS_SIZE_VER0: usize = 64; /* sizeof first published struct */
pub const CLONE_ARGS_SIZE_VER1: usize = 80; /* sizeof second published struct */
pub const CLONE_ARGS_SIZE_VER2: usize = 88; /* sizeof third published struct */

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CloneArgs {
    pub flags: u64,
    pub pidfd: u64,
    pub child_tid: u64,
    pub parent_tid: u64,
    pub exit_signal: u64,
    pub stack: u64,
    pub stack_size: u64,
    pub tls: u64,
    pub set_tid: u64,
    pub set_tid_size: u64,
    pub cgroup: u64,
}

impl CloneArgs {
    pub fn check_valid(&self, size: usize) -> Result<(), SystemError> {
        if self.set_tid_size > MAX_PID_NS_LEVEL as u64 {
            return Err(SystemError::EINVAL);
        }
        if self.set_tid == 0 && self.set_tid_size > 0 {
            return Err(SystemError::EINVAL);
        }
        if self.set_tid > 0 && self.set_tid_size == 0 {
            return Err(SystemError::EINVAL);
        }
        if self.exit_signal > 0xFF || self.exit_signal <= MAX_SIG_NUM as u64 {
            return Err(SystemError::EINVAL);
        }
        if self.flags & CloneFlags::CLONE_INTO_CGROUP.bits() != 0
            && (self.cgroup > i32::MAX as u64 || size < CLONE_ARGS_SIZE_VER2)
        {
            return Err(SystemError::EINVAL);
        }
        Ok(())
    }
}

pub fn do_clone(clone_args: KernelCloneArgs, frame: &mut TrapFrame) -> Result<usize, SystemError> {
    let flags = clone_args.flags;

    let vfork = Arc::new(Completion::new());

    if flags.contains(CloneFlags::CLONE_PIDFD) && flags.contains(CloneFlags::CLONE_PARENT_SETTID) {
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
