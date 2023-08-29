use alloc::{string::String, vec::Vec};

use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    exception::InterruptArch,
    include::bindings::bindings::{USER_CS, USER_DS},
    mm::ucontext::AddressSpace,
    process::{
        exec::{load_binary_file, ExecParam, ExecParamFlags},
        fork::CloneFlags,
        ProcessManager,
    },
    syscall::{Syscall, SystemError},
};

impl Syscall {
    pub fn fork(frame: &mut TrapFrame) -> Result<usize, SystemError> {
        ProcessManager::fork(frame, CloneFlags::empty()).map(|pid| pid.into())
    }

    pub fn vfork(frame: &mut TrapFrame) -> Result<usize, SystemError> {
        ProcessManager::fork(
            frame,
            CloneFlags::CLONE_VM | CloneFlags::CLONE_FS | CloneFlags::CLONE_SIGNAL,
        )
        .map(|pid| pid.into())
    }

    pub fn do_execve(
        path: String,
        argv: Vec<String>,
        envp: Vec<String>,
        regs: &mut TrapFrame,
    ) -> Result<(), SystemError> {
        // kdebug!(
        //     "tmp_rs_execve: path: {:?}, argv: {:?}, envp: {:?}\n",
        //     path,
        //     argv,
        //     envp
        // );
        // 关中断，防止在设置地址空间的时候，发生中断，然后进调度器，出现错误。
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        let pcb = ProcessManager::current_pcb();
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
        // kdebug!("Switch to new address space");

        // 切换到新的用户地址空间
        unsafe { address_space.read().user_mapper.utable.make_current() };

        drop(old_address_space);
        drop(irq_guard);
        // kdebug!("to load binary file");
        let mut param = ExecParam::new(path.as_str(), address_space.clone(), ExecParamFlags::EXEC);
        // 加载可执行文件
        let load_result = load_binary_file(&mut param)
            .unwrap_or_else(|e| panic!("Failed to load binary file: {:?}, path: {:?}", e, path));
        // kdebug!("load binary file done");

        param.init_info_mut().args = argv;
        param.init_info_mut().envs = envp;

        // 把proc_init_info写到用户栈上

        let (user_sp, argv_ptr) = unsafe {
            param
                .init_info()
                .push_at(
                    address_space
                        .write()
                        .user_stack_mut()
                        .expect("No user stack found"),
                )
                .expect("Failed to push proc_init_info to user stack")
        };

        // kdebug!("write proc_init_info to user stack done");

        // （兼容旧版libc）把argv的指针写到寄存器内
        // TODO: 改写旧版libc，不再需要这个兼容
        regs.rdi = param.init_info().args.len() as u64;
        regs.rsi = argv_ptr.data() as u64;

        // 设置系统调用返回时的寄存器状态
        // TODO: 中断管理重构后，这里的寄存器状态设置要删掉！！！改为对trap frame的设置。要增加架构抽象。
        regs.rsp = user_sp.data() as u64;
        regs.rbp = user_sp.data() as u64;
        regs.rip = load_result.entry_point().data() as u64;

        regs.cs = USER_CS as u64 | 3;
        regs.ds = USER_DS as u64 | 3;
        regs.ss = USER_DS as u64 | 3;
        regs.es = 0;
        regs.rflags = 0x200;
        regs.rax = 1;

        // kdebug!("regs: {:?}\n", regs);

        // kdebug!(
        //     "tmp_rs_execve: done, load_result.entry_point()={:?}",
        //     load_result.entry_point()
        // );
        return Ok(());
    }
}
