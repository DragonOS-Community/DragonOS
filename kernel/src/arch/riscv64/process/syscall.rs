use alloc::{ffi::CString, string::String, vec::Vec};
use riscv::register::sstatus::{FS, SPP};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    exception::InterruptArch,
    mm::ucontext::AddressSpace,
    process::{
        exec::{load_binary_file, ExecParam, ExecParamFlags},
        ProcessManager,
    },
    syscall::Syscall,
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
        // crate::debug!(
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

        regs.a0 = param.init_info().args.len();
        regs.a1 = argv_ptr.data();

        // 设置系统调用返回时的寄存器状态
        regs.sp = user_sp.data();

        regs.epc = load_result.entry_point().data();
        regs.status.update_spp(SPP::User);
        regs.status.update_fs(FS::Clean);
        regs.status.update_sum(true);

        drop(param);

        return Ok(());
    }

    /// ## 用于控制和查询与体系结构相关的进程特定选项
    #[allow(dead_code)]
    pub fn arch_prctl(_option: usize, _arg2: usize) -> Result<usize, SystemError> {
        unimplemented!("Syscall::arch_prctl")
    }
}
