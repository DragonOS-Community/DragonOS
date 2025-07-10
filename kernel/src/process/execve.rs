use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::ipc::signal_types::SignalStruct;
use crate::process::exec::{load_binary_file, ExecParam, ExecParamFlags};
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use crate::{libs::rand::rand_bytes, mm::ucontext::AddressSpace};

use crate::arch::interrupt::TrapFrame;
use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

pub fn do_execve(
    path: String,
    argv: Vec<CString>,
    envp: Vec<CString>,
    regs: &mut TrapFrame,
) -> Result<(), SystemError> {
    let address_space = AddressSpace::new(true).expect("Failed to create new address space");
    // debug!("to load binary file");
    let mut param = ExecParam::new(path.as_str(), address_space.clone(), ExecParamFlags::EXEC)?;
    let old_vm = do_execve_switch_user_vm(address_space.clone());

    // 加载可执行文件
    let load_result = load_binary_file(&mut param).inspect_err(|_| {
        if let Some(old_vm) = old_vm {
            do_execve_switch_user_vm(old_vm);
        }
    })?;

    // log::debug!("load binary file done");
    // debug!("argv: {:?}, envp: {:?}", argv, envp);
    param.init_info_mut().args = argv;
    param.init_info_mut().envs = envp;
    // // 生成16字节随机数
    param.init_info_mut().rand_num = rand_bytes::<16>();

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
            .init_info_mut()
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

    Syscall::arch_do_execve(regs, &param, &load_result, user_sp, argv_ptr)
}

/// 切换用户虚拟内存空间
///
/// 该函数用于在执行系统调用 `execve` 时切换用户进程的虚拟内存空间。
///
/// # 参数
/// - `new_vm`: 新的用户地址空间，类型为 `Arc<AddressSpace>`。
///
/// # 返回值
/// - 返回旧的用户地址空间的引用，类型为 `Option<Arc<AddressSpace>>`。
///
/// # 错误处理
/// 如果地址空间切换失败，函数会触发断言失败，并输出错误信息。
fn do_execve_switch_user_vm(new_vm: Arc<AddressSpace>) -> Option<Arc<AddressSpace>> {
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
    unsafe {
        basic_info.set_user_vm(Some(new_vm.clone()));
    }

    // to avoid deadlock
    drop(basic_info);

    assert!(
        AddressSpace::is_current(&new_vm),
        "Failed to set address space"
    );
    // debug!("Switch to new address space");

    // 切换到新的用户地址空间
    unsafe { new_vm.read().user_mapper.utable.make_current() };

    drop(irq_guard);

    old_address_space
}

/// todo: 该函数未正确实现
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/fs/exec.c?fi=begin_new_exec#1244
pub fn begin_new_exec(_param: &mut ExecParam) -> Result<(), SystemError> {
    de_thread()?;

    Ok(())
}

/// todo: 该函数未正确实现
/// https://code.dragonos.org.cn/xref/linux-6.1.9/fs/exec.c?fi=begin_new_exec#1042
fn de_thread() -> Result<(), SystemError> {
    *ProcessManager::current_pcb().sig_struct_irqsave() = SignalStruct::default();

    Ok(())
}
