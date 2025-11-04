use alloc::sync::Arc;
use system_error::SystemError;
use x86::{Ring, segmentation::SegmentSelector};

use crate::{
    arch::{
        CurrentIrqArch, interrupt::TrapFrame, process::table::{USER_CS, USER_DS}
    }, exception::InterruptArch, mm::VirtAddr, process::{
        ProcessControlBlock, ProcessManager, exec::{BinaryLoaderResult, ExecParam}
    }, syscall::{Syscall, user_access::UserBufferWriter}
};

const X86_USER_SPACE_MAX: usize = 0x0000_7fff_ffff_f000;

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
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/arch/x86/kernel/process_64.c#913
    pub fn arch_prctl(option: usize, arg2: usize) -> Result<usize, SystemError> {
        let pcb = ProcessManager::current_pcb();
        let result = Self::do_arch_prctl_64(&pcb, option, arg2, true);

        if let Err(SystemError::EINVAL) = result {
            Self::do_arch_prctl_common(option, arg2)?;
            Ok(0)
        } else {
            result
        }
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
                if arg2 >= X86_USER_SPACE_MAX {
                    return Err(SystemError::EPERM);
                }

                arch_info.fsbase = arg2;
                arch_info.fs = SegmentSelector::new(0, Ring::Ring0); // 清零选择子

                if pcb.raw_pid() == ProcessManager::current_pcb().raw_pid() {
                    // 先加载段选择子为 0
                    unsafe {
                        x86::segmentation::load_fs(SegmentSelector::new(0, Ring::Ring0));
                    }
                    // 再设置 base
                    unsafe { arch_info.restore_fsbase() }
                }
            }

            ARCH_SET_GS => {
                if arg2 >= X86_USER_SPACE_MAX {
                    return Err(SystemError::EPERM);
                }

                arch_info.gsbase = arg2;
                arch_info.gs = SegmentSelector::new(0, Ring::Ring0); // 清零选择子

                if pcb.raw_pid() == ProcessManager::current_pcb().raw_pid() {
                    // GS 的处理更复杂，需要禁用中断
                    let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
                    unsafe {
                        x86::segmentation::load_gs(SegmentSelector::new(0, Ring::Ring0));
                    }
                    unsafe { arch_info.restore_gsbase() }
                    drop(irq_guard);
                }
            }
            _ => {
                return Err(SystemError::EINVAL);
            }
        }
        Ok(0)
    }

    #[allow(dead_code)]
    pub fn do_arch_prctl_common(option: usize, arg2: usize) -> Result<usize, SystemError> {
        // Don't use 0x3001-0x3004 because of old glibcs
        if (0x3001..=0x3004).contains(&option) {
            return Err(SystemError::EINVAL);
        }

        todo!(
            "do_arch_prctl_common not unimplemented, option: {}, arg2: {}",
            option,
            arg2
        );
    }
}

pub const ARCH_SET_GS: usize = 0x1001;
pub const ARCH_SET_FS: usize = 0x1002;
pub const ARCH_GET_FS: usize = 0x1003;
pub const ARCH_GET_GS: usize = 0x1004;
