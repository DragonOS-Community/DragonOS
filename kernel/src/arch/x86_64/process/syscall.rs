use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

use crate::{
    arch::{
        interrupt::TrapFrame,
        process::table::{USER_CS, USER_DS},
        CurrentIrqArch,
    },
    exception::InterruptArch,
    mm::ucontext::AddressSpace,
    process::{
        exec::{load_binary_file, ExecParam, ExecParamFlags},
        ProcessControlBlock, ProcessManager,
    },
    syscall::{user_access::UserBufferWriter, Syscall},
};

impl Syscall {
    pub fn do_execve(
        path: String,
        argv: Vec<CString>,
        envp: Vec<CString>,
        regs: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        // 关中断，防止在设置地址空间的时候，发生中断，然后进调度器，出现错误。
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let pcb = ProcessManager::current_pcb();
        // log::debug!(
        //     "pid: {:?}  do_execve: path: {:?}, argv: {:?}, envp: {:?}\n",
        //     pcb.pid(),
        //     path,
        //     argv,
        //     envp
        // );

        let mut basic_info = pcb.basic_mut();
        // 暂存原本的用户地址空间的引用(因为如果在切换页表之前释放了它，可能会造成内存use after free)
        let old_address_space = basic_info.user_vm();

        // 在pcb中原来的用户地址空间
        unsafe {
            basic_info.set_user_vm(None);
        }
        // 创建新的地址空间并设置为当前地址空间
        let address_space = AddressSpace::new(true).expect("Failed to create new address space");
        unsafe {
            basic_info.set_user_vm(Some(address_space.clone()));
        }

        // to avoid deadlock
        drop(basic_info);

        assert!(
            AddressSpace::is_current(&address_space),
            "Failed to set address space"
        );
        // debug!("Switch to new address space");

        // 切换到新的用户地址空间
        unsafe { address_space.read().user_mapper.utable.make_current() };

        drop(old_address_space);
        drop(irq_guard);
        // debug!("to load binary file");
        let mut param = ExecParam::new(path.as_str(), address_space.clone(), ExecParamFlags::EXEC)?;

        // 加载可执行文件
        let load_result = load_binary_file(&mut param)?;
        // debug!("load binary file done");
        // debug!("argv: {:?}, envp: {:?}", argv, envp);
        param.init_info_mut().args = argv;
        param.init_info_mut().envs = envp;

        // 把proc_init_info写到用户栈上
        let mut ustack_message = unsafe {
            address_space
                .write()
                .user_stack_mut()
                .expect("No user stack found")
                .clone_info_only()
        };
        let (user_sp, argv_ptr) = unsafe {
            param
                .init_info()
                .push_at(
                    // address_space
                    //     .write()
                    //     .user_stack_mut()
                    //     .expect("No user stack found"),
                    &mut ustack_message,
                )
                .expect("Failed to push proc_init_info to user stack")
        };
        address_space.write().user_stack = Some(ustack_message);

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

        drop(param);

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
