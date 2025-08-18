use alloc::sync::Arc;

use system_error::SystemError;

use crate::process::{
    fork::CloneFlags,
    namespace::nsproxy::{switch_task_namespaces, NsProxy},
    ProcessManager,
};

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/fork.c#3385
pub fn ksys_unshare(flags: CloneFlags) -> Result<(), SystemError> {
    // 检查 unshare 标志位
    check_unshare_flags(flags)?;

    let new_nsproxy = unshare_nsproxy_namespaces(flags)?;

    if let Some(new_nsproxy) = new_nsproxy {
        // 更新当前进程的 Namespace 代理
        let current_pcb = ProcessManager::current_pcb();
        switch_task_namespaces(&current_pcb, new_nsproxy)?;
    }
    // TODO: 处理其他命名空间的 unshare 操作
    // CLONE_NEWNS, CLONE_FS, CLONE_FILES, CLONE_SIGHAND, CLONE_VM, CLONE_THREAD, CLONE_SYSVSEM,
    // CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER, CLONE_NEWNET, CLONE_NEWCGROUP, CLONE_NEWTIME

    Ok(())
}

#[inline(never)]
fn unshare_nsproxy_namespaces(
    unshare_flags: CloneFlags,
) -> Result<Option<Arc<NsProxy>>, SystemError> {
    const ALL_VALID_FLAGS: CloneFlags = CloneFlags::from_bits_truncate(
        CloneFlags::CLONE_NEWNS.bits()
            | CloneFlags::CLONE_NEWUTS.bits()
            | CloneFlags::CLONE_NEWIPC.bits()
            | CloneFlags::CLONE_NEWNET.bits()
            | CloneFlags::CLONE_NEWPID.bits()
            | CloneFlags::CLONE_NEWCGROUP.bits()
            | CloneFlags::CLONE_NEWTIME.bits(),
    );
    if !unshare_flags.intersects(ALL_VALID_FLAGS) {
        return Ok(None);
    }

    // 获取当前进程的 PCB
    let current_pcb = ProcessManager::current_pcb();
    let user_ns = ProcessManager::current_user_ns();

    let nsproxy = super::nsproxy::create_new_namespaces(&unshare_flags, &current_pcb, user_ns)?;
    return Ok(Some(nsproxy));
}

#[inline(never)]
fn check_unshare_flags(flags: CloneFlags) -> Result<(), SystemError> {
    // 检查无效的标志位
    const ALL_VALID_FLAGS: CloneFlags = CloneFlags::from_bits_truncate(
        CloneFlags::CLONE_NEWNS.bits()
            | CloneFlags::CLONE_NEWCGROUP.bits()
            | CloneFlags::CLONE_NEWUTS.bits()
            | CloneFlags::CLONE_NEWIPC.bits()
            | CloneFlags::CLONE_NEWUSER.bits()
            | CloneFlags::CLONE_NEWPID.bits()
            | CloneFlags::CLONE_NEWNET.bits()
            | CloneFlags::CLONE_NEWTIME.bits()
            | CloneFlags::CLONE_FS.bits()
            | CloneFlags::CLONE_FILES.bits()
            | CloneFlags::CLONE_SIGHAND.bits()
            | CloneFlags::CLONE_VM.bits()
            | CloneFlags::CLONE_THREAD.bits()
            | CloneFlags::CLONE_SYSVSEM.bits(),
    );

    if flags.intersects(!ALL_VALID_FLAGS) {
        return Err(SystemError::EINVAL);
    }

    let current_pcb = ProcessManager::current_pcb();

    // 如果请求 unshare CLONE_THREAD, CLONE_SIGHAND 或 CLONE_VM，
    // 必须确保线程组为空（即只有一个线程）
    if flags.intersects(CloneFlags::CLONE_THREAD | CloneFlags::CLONE_SIGHAND | CloneFlags::CLONE_VM)
        && !current_pcb.threads_read_irqsave().thread_group_empty()
    {
        return Err(SystemError::EINVAL);
    }

    // 如果请求 unshare CLONE_SIGHAND 或 CLONE_VM，
    // 必须确保信号处理结构的引用计数为1
    if flags.intersects(CloneFlags::CLONE_SIGHAND | CloneFlags::CLONE_VM) {
        let sighand_count = current_pcb
            .sig_struct_irqsave()
            .cnt
            .load(core::sync::atomic::Ordering::SeqCst);
        if sighand_count > 1 {
            return Err(SystemError::EINVAL);
        }
    }

    // TODO: 如果请求 unshare CLONE_VM，
    // 必须确保当前进程是单线程进程
    // if flags.contains(CloneFlags::CLONE_VM) {
    //     if !current_pcb.thread_group_empty() {
    //         return Err(SystemError::EINVAL);
    //     }
    // }

    Ok(())
}
