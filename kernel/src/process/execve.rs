use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::filesystem::vfs::IndexNode;
use crate::libs::rwlock::RwLock;
use crate::process::exec::{
    load_binary_file_with_context, ExecContext, ExecParam, ExecParamFlags, LoadBinaryResult,
};
use crate::process::ProcessManager;
use crate::syscall::Syscall;
use crate::{libs::rand::rand_bytes, mm::ucontext::AddressSpace};

use crate::arch::interrupt::TrapFrame;
use alloc::{ffi::CString, sync::Arc, vec::Vec};
use system_error::SystemError;

/// 执行execve系统调用
///
/// ## 参数
/// - `file_inode`: 要执行的文件的inode
/// - `argv`: 参数列表
/// - `envp`: 环境变量列表
/// - `regs`: 陷入帧
///
/// ## 返回值
/// 成功时不返回（跳转到新程序），失败时返回错误
pub fn do_execve(
    file_inode: Arc<dyn IndexNode>,
    argv: Vec<CString>,
    envp: Vec<CString>,
    regs: &mut TrapFrame,
) -> Result<(), SystemError> {
    // 创建初始执行上下文
    let mut ctx = ExecContext::new();

    // 保存原始脚本路径（用于shebang场景）
    ctx.original_path = file_inode.absolute_path().ok();

    do_execve_internal(file_inode, argv, envp, regs, ctx)
}

/// execve的内部实现，支持递归执行（用于shebang）
///
/// ## 参数
/// - `file_inode`: 要执行的文件的inode
/// - `argv`: 参数列表
/// - `envp`: 环境变量列表
/// - `regs`: 陷入帧
/// - `ctx`: 执行上下文，用于跟踪递归深度
#[inline(never)]
fn do_execve_internal(
    file_inode: Arc<dyn IndexNode>,
    argv: Vec<CString>,
    envp: Vec<CString>,
    regs: &mut TrapFrame,
    ctx: ExecContext,
) -> Result<(), SystemError> {
    let address_space = AddressSpace::new(true).expect("Failed to create new address space");

    let mut param = ExecParam::new(
        file_inode.clone(),
        address_space.clone(),
        ExecParamFlags::EXEC,
    )?;

    // 预先设置args，以便shebang处理时可以访问原始参数
    param.init_info_mut().args = argv.clone();
    param.init_info_mut().envs = envp.clone();

    let old_vm = do_execve_switch_user_vm(address_space.clone());

    // 尝试加载二进制文件
    let load_result = load_binary_file_with_context(&mut param, &ctx);

    match load_result {
        Ok(LoadBinaryResult::Loaded(result)) => {
            // 正常ELF加载流程
            // 生成16字节随机数
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
                    .push_at(&mut ustack_message)
                    .expect("Failed to push proc_init_info to user stack")
            };
            address_space.write().user_stack = Some(ustack_message);

            // execve 成功后，如果是 vfork 创建的子进程，需要通知父进程继续执行
            // 在通知父进程之前，必须先清除 vfork_done，防止子进程退出时再次通知
            let pcb = ProcessManager::current_pcb();
            let vfork_done = pcb.thread.write_irqsave().vfork_done.take();

            if let Some(completion) = vfork_done {
                completion.complete_all();
            }

            // unshare fd_table if it's shared (CLONE_FILES case)
            // 参考 Linux: https://elixir.bootlin.com/linux/v6.1.9/source/fs/exec.c#L1857
            // "Ensure the files table is not shared"
            {
                let fd_table = pcb.fd_table();
                // 检查 fd_table 是否被共享 (Arc::strong_count() > 1)
                if Arc::strong_count(&fd_table) > 1 {
                    // fd_table 被共享，需要创建私有副本
                    let new_fd_table = fd_table.read().clone();
                    let new_fd_table = Arc::new(RwLock::new(new_fd_table));
                    pcb.basic_mut().set_fd_table(Some(new_fd_table));
                }
            }

            if pcb.sighand().is_shared() {
                // Linux出于进程和线程隔离，要确保在execve时，对共享的 SigHand 进行深拷贝
                // 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/fs/exec.c#1187
                let new_sighand = crate::ipc::sighand::SigHand::new();
                new_sighand.copy_handlers_from(&pcb.sighand());
                pcb.replace_sighand(new_sighand);
            }
            // 重置所有信号处理器为默认行为(SIG_DFL)，禁用并清空备用信号栈。
            pcb.flush_signal_handlers(false);
            *pcb.sig_altstack_mut() = crate::arch::SigStackArch::new();

            // 清除 rseq 状态（execve 后需要重新注册）
            crate::process::rseq::rseq_execve(&pcb);

            Syscall::arch_do_execve(regs, &param, &result, user_sp, argv_ptr)
        }

        Ok(LoadBinaryResult::NeedReexec {
            interpreter_inode,
            new_argv,
        }) => {
            // Shebang场景：需要递归执行解释器
            // 恢复旧的地址空间
            if let Some(old_vm) = old_vm {
                do_execve_switch_user_vm(old_vm);
            }

            // 增加递归深度并递归调用
            let new_ctx = ctx.increment_depth();

            do_execve_internal(interpreter_inode, new_argv, envp, regs, new_ctx)
        }

        Err(e) => {
            // 加载失败，恢复旧的地址空间
            if let Some(old_vm) = old_vm {
                do_execve_switch_user_vm(old_vm);
            }
            Err(e)
        }
    }
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

    // 切换到新的用户地址空间
    unsafe { new_vm.read().user_mapper.utable.make_current() };

    drop(irq_guard);

    old_address_space
}
