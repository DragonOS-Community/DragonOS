use crate::arch::CurrentIrqArch;
use crate::exception::InterruptArch;
use crate::filesystem::vfs::fcntl::AtFlags;
use crate::filesystem::vfs::open::{do_open_execat, do_open_execat_with_flags};
use crate::libs::rwsem::RwSem;
use crate::process::exec::{
    load_binary_file_with_context, ExecContext, ExecInterpFlags, ExecParam, ExecParamFlags,
    ExecStartInfo, LoadBinaryResult,
};
use crate::process::{ProcessControlBlock, ProcessManager};
use crate::syscall::Syscall;
use crate::{libs::rand::rand_bytes, mm::ucontext::AddressSpace};

use crate::arch::interrupt::TrapFrame;
use alloc::{ffi::CString, string::String, sync::Arc, vec::Vec};
use system_error::SystemError;

/// 执行execve系统调用
///
/// ## 参数
/// - `path`: 要执行的文件路径
/// - `argv`: 参数列表
/// - `envp`: 环境变量列表
/// - `regs`: 陷入帧
///
/// ## 返回值
/// 成功时不返回（跳转到新程序），失败时返回错误
pub fn do_execve(
    path: &str,
    argv: Vec<CString>,
    envp: Vec<CString>,
    regs: &mut TrapFrame,
) -> Result<(), SystemError> {
    let file = do_open_execat(AtFlags::AT_FDCWD.bits(), path)?;
    let start = ExecStartInfo::new(file, path.into(), path.into(), ExecInterpFlags::empty());

    do_execve_with_info(start, argv, envp, regs)
}

pub fn do_execveat(
    dirfd: i32,
    path: &str,
    argv: Vec<CString>,
    envp: Vec<CString>,
    flags: AtFlags,
    regs: &mut TrapFrame,
) -> Result<(), SystemError> {
    let file = do_open_execat_with_flags(dirfd, path, flags)?;
    let start = ExecStartInfo::new(
        file,
        exec_visible_name(dirfd, path),
        exec_initial_execfn(dirfd, path),
        exec_path_interp_flags(dirfd, path),
    );

    do_execve_with_info(start, argv, envp, regs)
}

pub fn do_execve_with_info(
    start: ExecStartInfo,
    argv: Vec<CString>,
    envp: Vec<CString>,
    regs: &mut TrapFrame,
) -> Result<(), SystemError> {
    do_execve_internal(start, argv, envp, regs, ExecContext::new())
}

pub fn exec_visible_name(dirfd: i32, path: &str) -> String {
    if dirfd == AtFlags::AT_FDCWD.bits() || path.starts_with('/') {
        path.into()
    } else if path.is_empty() {
        alloc::format!("/dev/fd/{dirfd}")
    } else {
        alloc::format!("/dev/fd/{dirfd}/{path}")
    }
}

pub fn exec_initial_execfn(dirfd: i32, path: &str) -> String {
    exec_visible_name(dirfd, path)
}

fn exec_path_interp_flags(dirfd: i32, path: &str) -> ExecInterpFlags {
    if dirfd == AtFlags::AT_FDCWD.bits() || path.starts_with('/') {
        return ExecInterpFlags::empty();
    }

    let cloexec = ProcessManager::current_pcb()
        .fd_table()
        .read()
        .get_cloexec(dirfd);
    if cloexec {
        ExecInterpFlags::PATH_INACCESSIBLE
    } else {
        ExecInterpFlags::empty()
    }
}

/// execve的内部实现，支持递归执行（用于shebang）
///
/// ## 参数
/// - `file`: 已打开的可执行文件
/// - `argv`: 参数列表
/// - `envp`: 环境变量列表
/// - `regs`: 陷入帧
/// - `ctx`: 执行上下文，用于跟踪递归深度
#[inline(never)]
fn do_execve_internal(
    start: ExecStartInfo,
    argv: Vec<CString>,
    envp: Vec<CString>,
    regs: &mut TrapFrame,
    ctx: ExecContext,
) -> Result<(), SystemError> {
    let address_space = AddressSpace::new(true).expect("Failed to create new address space");

    let mut param = ExecParam::new(
        start.file(),
        address_space.clone(),
        ExecParamFlags::EXEC,
        CString::new(start.filename()).map_err(|_| SystemError::EINVAL)?,
        CString::new(start.execfn()).map_err(|_| SystemError::EINVAL)?,
        start.interp_flags(),
    );

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

            let pcb = ProcessManager::current_pcb();

            // unshare fd_table if it's shared (CLONE_FILES case)
            // 参考 Linux: https://elixir.bootlin.com/linux/v6.1.9/source/fs/exec.c#L1857
            // "Ensure the files table is not shared"
            {
                // 注意：不能先调用 pcb.fd_table() 再判断 strong_count，
                // 因为 fd_table() 会克隆 Arc，导致计数至少 +1，误判为“被共享”。
                let need_unshare = pcb.basic().fd_table_is_shared();
                if need_unshare {
                    // fd_table 被共享，需要创建私有副本
                    let fd_table = pcb.fd_table();
                    let new_fd_table = fd_table.read().clone();
                    let new_fd_table = Arc::new(RwSem::new(new_fd_table));
                    pcb.basic_mut().set_fd_table(Some(new_fd_table));
                }
            }

            // close-on-exec 必须属于成功 exec 的 commit 过程，不能留在 syscall wrapper 尾部。
            pcb.fd_table().write().close_on_exec();

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

            let executable_path = param
                .file_ref()
                .inode()
                .absolute_path()
                .unwrap_or_else(|_| param.filename().to_string_lossy().into_owned());
            pcb.basic_mut()
                .set_name(ProcessControlBlock::generate_name(&executable_path));
            pcb.set_execute_path(executable_path);
            pcb.set_cmdline_from_argv(&param.init_info().args);

            // vfork 父进程必须在 child 完成 exec commit 后再恢复。
            // 否则父子仍可能共享 files_struct，child 的 close_on_exec() 会污染父进程。
            let vfork_done = pcb.thread.write_irqsave().vfork_done.take();
            let exec_ret = Syscall::arch_do_execve(regs, &param, &result, user_sp, argv_ptr);
            if exec_ret.is_ok() {
                if let Some(completion) = vfork_done {
                    completion.complete_all();
                }
            }
            exec_ret
        }

        Ok(LoadBinaryResult::NeedReexec { next, new_argv }) => {
            // Shebang场景：需要递归执行解释器
            // 恢复旧的地址空间
            if let Some(old_vm) = old_vm {
                do_execve_switch_user_vm(old_vm);
            }

            // 增加递归深度并递归调用
            let new_ctx = ctx.increment_depth();

            do_execve_internal(next, new_argv, envp, regs, new_ctx)
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

    let cpu = crate::smp::core::smp_get_processor_id();

    let mut basic_info = pcb.basic_mut();
    // 暂存原本的用户地址空间的引用(因为如果在切换页表之前释放了它，可能会造成内存use after free)
    let old_address_space = basic_info.user_vm();

    // INV-1: when execve switches mm, first clear this CPU from the old mm's active_cpus,
    // then switch the hardware page table, then add this CPU to the new mm's active_cpus,
    // and finally update per-CPU TlbState.
    // Note: on the execve path the old/new mm are always different (the new mm is a freshly
    // created AddressSpace::new result).
    if let Some(old_vm) = old_address_space.as_ref() {
        old_vm.active_cpus_clear(cpu);
    }

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
    unsafe { new_vm.make_current() };

    new_vm.active_cpus_set(cpu);
    unsafe { crate::mm::tlb::tlb_state_set_loaded_mm(new_vm.clone()) };

    drop(irq_guard);

    old_address_space
}
