use alloc::sync::Arc;
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
    },
    mm::VirtAddr,
    process::{
        exec::{BinaryLoaderResult, ExecParam},
        ProcessControlBlock, ProcessManager,
    },
    syscall::{user_access::UserBufferWriter, Syscall},
};

impl Syscall {
    pub fn arch_do_execve(
        regs: &mut TrapFrame,
        param: &ExecParam,
        load_result: &BinaryLoaderResult,
        user_sp: VirtAddr,
        argv_ptr: VirtAddr,
    ) -> Result<(), SystemError> {
        // debug!("write proc_init_info to user stack done");

        // （兼容旧版libc）把argv的指针写到寄存器内
        // TODO: 改写旧版libc，不再需要这个兼容
        regs.rdi = param.init_info().args.len() as u64;
        regs.rsi = argv_ptr.data() as u64;

        // 设置系统调用返回时的寄存器状态
        // TODO: 中断管理重构后，这里的寄存器状态设置要删掉！！！改为对trap frame的设置。要增加架构抽象。
        regs.rsp = user_sp.data() as u64;
        regs.rbp = user_sp.data() as u64;
        regs.rip = load_result.entry_point().data() as u64;

        regs.cs = USER_CS.bits() as u64;
        regs.ds = USER_DS.bits() as u64;
        regs.ss = USER_DS.bits() as u64;
        regs.es = 0;
        regs.rflags = 0x200;
        regs.rax = 1;

        // debug!("regs: {:?}\n", regs);

        // crate::debug!(
        //     "tmp_rs_execve: done, load_result.entry_point()={:?}",
        //     load_result.entry_point()
        // );

        return Ok(());
    }

    /// ## 用于控制和查询与体系结构相关的进程特定选项
    pub fn arch_prctl(option: usize, arg2: usize) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        if let Err(SystemError::EINVAL) = Self::do_arch_prctl_64(&pcb, option, arg2, true) {
            Self::do_arch_prctl_common(option, arg2)?;
        }
        Ok(0)
    }

    /// ## 64位下控制fs/gs base寄存器的方法
    pub fn do_arch_prctl_64(
        pcb: &Arc<ProcessControlBlock>,
        option: usize,
        arg2: usize,
        from_user: bool,
    ) -> Result<usize, SystemError> {
        let mut arch_info = pcb.arch_info_irqsave();
        match option {
            ARCH_GET_FS => {
                unsafe { arch_info.save_fsbase() };
                let mut writer = UserBufferWriter::new(
                    arg2 as *mut usize,
                    core::mem::size_of::<usize>(),
                    from_user,
                )?;
                writer.copy_one_to_user(&arch_info.fsbase, 0)?;
            }
            ARCH_GET_GS => {
                unsafe { arch_info.save_gsbase() };
                let mut writer = UserBufferWriter::new(
                    arg2 as *mut usize,
                    core::mem::size_of::<usize>(),
                    from_user,
                )?;
                writer.copy_one_to_user(&arch_info.gsbase, 0)?;
            }
            ARCH_SET_FS => {
                arch_info.fsbase = arg2;
                // 如果是当前进程则直接写入寄存器
                if pcb.pid() == ProcessManager::current_pcb().pid() {
                    unsafe { arch_info.restore_fsbase() }
                }
            }
            ARCH_SET_GS => {
                arch_info.gsbase = arg2;
                if pcb.pid() == ProcessManager::current_pcb().pid() {
                    unsafe { arch_info.restore_gsbase() }
                }
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
        Ok(0)
    }

    #[allow(dead_code)]
    pub fn do_arch_prctl_common(_option: usize, _arg2: usize) -> Result<usize, SystemError> {
        todo!("do_arch_prctl_common not unimplemented");
    }
}

pub const ARCH_SET_GS: usize = 0x1001;
pub const ARCH_SET_FS: usize = 0x1002;
pub const ARCH_GET_FS: usize = 0x1003;
pub const ARCH_GET_GS: usize = 0x1004;
