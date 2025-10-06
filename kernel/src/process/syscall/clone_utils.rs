use core::intrinsics::unlikely;

use crate::arch::interrupt::TrapFrame;
use crate::arch::ipc::signal::Signal;
use crate::arch::ipc::signal::MAX_SIG_NUM;
use crate::arch::MMArch;
use crate::filesystem::procfs::procfs_register_pid;
use crate::mm::{MemoryManagementArch, VirtAddr};
use crate::process::fork::{CloneFlags, KernelCloneArgs, MAX_PID_NS_LEVEL};
use crate::process::{KernelStack, ProcessControlBlock, ProcessManager};
use crate::sched::completion::Completion;
use crate::syscall::user_access::{UserBufferReader, UserBufferWriter};
use alloc::{string::ToString, sync::Arc};
use system_error::SystemError;

#[repr(C)]
#[derive(Clone, Copy)]
struct PosixCloneArgs {
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

impl PosixCloneArgs {
    const CLONE_ARGS_SIZE_VER0: usize = 64; /* sizeof first published struct */
    const CLONE_ARGS_SIZE_VER1: usize = 80; /* sizeof second published struct */
    const CLONE_ARGS_SIZE_VER2: usize = 88; /* sizeof third published struct */

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
        if self.exit_signal >= MAX_SIG_NUM as u64 {
            return Err(SystemError::EINVAL);
        }
        if self.flags & CloneFlags::CLONE_INTO_CGROUP.bits() != 0
            && (self.cgroup > i32::MAX as u64 || size < Self::CLONE_ARGS_SIZE_VER2)
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

impl KernelCloneArgs {
    pub fn copy_clone_args_from_user(
        &mut self,
        uargs_ptr: usize,
        size: usize,
    ) -> Result<(), SystemError> {
        // 编译时检查
        use kdepends::memoffset::offset_of;
        const {
            assert!(
                offset_of!(PosixCloneArgs, tls) + core::mem::size_of::<u64>()
                    == PosixCloneArgs::CLONE_ARGS_SIZE_VER0
            )
        };
        const {
            assert!(
                offset_of!(PosixCloneArgs, set_tid_size) + core::mem::size_of::<u64>()
                    == PosixCloneArgs::CLONE_ARGS_SIZE_VER1
            )
        };
        const {
            assert!(
                offset_of!(PosixCloneArgs, cgroup) + core::mem::size_of::<u64>()
                    == PosixCloneArgs::CLONE_ARGS_SIZE_VER2
            )
        };
        const { assert!(core::mem::size_of::<PosixCloneArgs>() == PosixCloneArgs::CLONE_ARGS_SIZE_VER2) };

        if unlikely(size as u64 > MMArch::PAGE_SIZE as u64) {
            return Err(SystemError::E2BIG);
        }
        if unlikely(size < PosixCloneArgs::CLONE_ARGS_SIZE_VER0) {
            return Err(SystemError::EINVAL);
        }

        // 仅根据用户提供的size从用户态读取，避免越界读取
        let bufreader = UserBufferReader::new(uargs_ptr as *const u8, size, true)?;

        // 默认零初始化整个结构体，然后仅覆盖前size字节，保证未提供部分为0
        let mut args: PosixCloneArgs = unsafe { core::mem::MaybeUninit::zeroed().assume_init() };
        let copy_size = core::cmp::min(size, core::mem::size_of::<PosixCloneArgs>());
        let args_prefix = unsafe {
            core::slice::from_raw_parts_mut(
                (&mut args as *mut PosixCloneArgs) as *mut u8,
                copy_size,
            )
        };
        // 从用户空间拷贝size字节到结构体前缀
        bufreader.copy_from_user::<u8>(args_prefix, 0)?;

        args.check_valid(size)?;

        self.flags = CloneFlags::from_bits_truncate(args.flags);
        self.pidfd = VirtAddr::new(args.pidfd as usize);
        self.child_tid = VirtAddr::new(args.child_tid as usize);
        self.parent_tid = VirtAddr::new(args.parent_tid as usize);
        self.exit_signal = Signal::from(args.exit_signal as i32);
        self.stack = args.stack as usize;
        self.stack_size = args.stack_size as usize;
        self.tls = args.tls as usize;
        self.set_tid_size = args.set_tid_size as usize;
        self.cgroup = args.cgroup as i32;

        if self.set_tid_size > 0 {
            let bufreader = UserBufferReader::new(
                args.set_tid as *const core::ffi::c_int,
                core::mem::size_of::<core::ffi::c_int>() * self.set_tid_size,
                true,
            )?;
            for i in 0..self.set_tid_size {
                let byte_offset = i * core::mem::size_of::<core::ffi::c_int>();
                let tid = *bufreader.read_one_from_user::<core::ffi::c_int>(byte_offset)?;
                self.set_tid.push(tid as usize);
            }
        }

        Ok(())
    }
}
