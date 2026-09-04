use alloc::sync::Arc;

use system_error::SystemError;

use crate::{
    filesystem::fs::FsStruct,
    process::{
        cred::{ns_capable, CAPFlags, Cred},
        fork::CloneFlags,
        lock_fs_refs_copy,
        namespace::nsproxy::{create_new_namespaces, NsProxy, PreparedNamespaceInstall},
        FsRefsReadGuard, ProcessControlBlock, ProcessManager,
    },
};

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/fork.c#3385
pub fn ksys_unshare(flags: CloneFlags) -> Result<(), SystemError> {
    let flags = normalize_unshare_flags(flags);

    // 检查 unshare 标志位
    check_unshare_flags(flags)?;

    let current_pcb = ProcessManager::current_pcb();
    let new_cred = unshare_user_cred(flags, &current_pcb)?.map(Cred::new_arc);
    let fs_refs = lock_fs_refs_copy();
    let new_fs = unshare_fs_struct(flags, &current_pcb, &fs_refs)?;
    let new_nsproxy =
        unshare_nsproxy_namespaces(flags, &current_pcb, new_cred.as_deref(), new_fs.as_ref())?;
    let prepared =
        prepare_unshare_install(&current_pcb, flags, new_fs, new_nsproxy, new_cred, &fs_refs)?;
    prepared.commit(&current_pcb, &fs_refs)?;

    // TODO: 处理其他命名空间的 unshare 操作
    // CLONE_FS, CLONE_FILES, CLONE_SIGHAND, CLONE_VM, CLONE_THREAD, CLONE_SYSVSEM,
    // CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER, CLONE_NEWNET, CLONE_NEWCGROUP, CLONE_NEWTIME

    Ok(())
}

fn prepare_unshare_install(
    current: &Arc<ProcessControlBlock>,
    flags: CloneFlags,
    new_fs: Option<Arc<FsStruct>>,
    new_nsproxy: Option<Arc<NsProxy>>,
    new_cred: Option<Arc<Cred>>,
    fs_refs: &FsRefsReadGuard,
) -> Result<PreparedNamespaceInstall, SystemError> {
    let new_nsproxy = new_nsproxy.unwrap_or_else(|| current.nsproxy());
    let detach_sysvsem = flags.intersects(CloneFlags::CLONE_SYSVSEM | CloneFlags::CLONE_NEWIPC);
    PreparedNamespaceInstall::prepare_for_unshare(
        current,
        new_nsproxy,
        new_fs,
        new_cred,
        detach_sysvsem,
        fs_refs,
    )
}

#[inline(always)]
fn normalize_unshare_flags(mut flags: CloneFlags) -> CloneFlags {
    if flags.contains(CloneFlags::CLONE_NEWUSER) {
        flags |= CloneFlags::CLONE_THREAD | CloneFlags::CLONE_FS;
    }
    if flags.contains(CloneFlags::CLONE_VM) {
        flags |= CloneFlags::CLONE_SIGHAND;
    }
    if flags.contains(CloneFlags::CLONE_SIGHAND) {
        flags |= CloneFlags::CLONE_THREAD;
    }
    if flags.contains(CloneFlags::CLONE_NEWNS) {
        flags |= CloneFlags::CLONE_FS;
    }
    flags
}

#[inline(never)]
fn unshare_user_cred(
    unshare_flags: CloneFlags,
    current_pcb: &Arc<crate::process::ProcessControlBlock>,
) -> Result<Option<Cred>, SystemError> {
    if !unshare_flags.contains(CloneFlags::CLONE_NEWUSER) {
        return Ok(None);
    }

    let mut new_cred = (*current_pcb.cred()).clone();
    let new_user_ns =
        crate::process::namespace::user_namespace::UserNamespace::create_user_ns(&new_cred)?;
    crate::process::cred::set_cred_user_ns(&mut new_cred, new_user_ns);
    Ok(Some(new_cred))
}

#[inline(never)]
fn unshare_fs_struct(
    unshare_flags: CloneFlags,
    current_pcb: &Arc<crate::process::ProcessControlBlock>,
    _fs_refs: &crate::process::FsRefsReadGuard,
) -> Result<Option<Arc<FsStruct>>, SystemError> {
    if !unshare_flags.contains(CloneFlags::CLONE_FS) {
        return Ok(None);
    }

    if !current_pcb.fs_struct_is_shared() {
        return Ok(None);
    }

    let current_fs = current_pcb.fs_struct();
    Ok(Some(Arc::new((*current_fs).clone())))
}

#[inline(never)]
fn unshare_nsproxy_namespaces(
    unshare_flags: CloneFlags,
    current_pcb: &Arc<crate::process::ProcessControlBlock>,
    new_cred: Option<&Cred>,
    _new_fs: Option<&Arc<FsStruct>>,
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

    let user_ns = new_cred
        .map(|cred| cred.user_ns.clone())
        .unwrap_or_else(ProcessManager::current_user_ns);

    if !ns_capable(&user_ns, CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }

    let nsproxy = create_new_namespaces(&unshare_flags, current_pcb, user_ns)?;

    Ok(Some(nsproxy))
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
    if flags.intersects(CloneFlags::CLONE_SIGHAND | CloneFlags::CLONE_VM)
        && current_pcb.sighand().is_shared()
    {
        return Err(SystemError::EINVAL);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{KernelStack, ProcessControlBlock};

    fn test_pcb() -> Arc<ProcessControlBlock> {
        ProcessControlBlock::new_idle(0, KernelStack::new().unwrap())
    }

    #[test]
    fn unshare_sysvsem_detaches_even_when_ipc_namespace_is_unchanged() {
        let pcb = test_pcb();
        let ipc_ns = pcb.nsproxy().ipc_ns.clone();
        let old_group = pcb.ensure_sem_undo_group(&ipc_ns).unwrap();
        let fs_refs = lock_fs_refs_copy();

        let prepared =
            prepare_unshare_install(&pcb, CloneFlags::CLONE_SYSVSEM, None, None, None, &fs_refs)
                .unwrap();
        prepared.commit(&pcb, &fs_refs).unwrap();

        assert!(pcb.sem_undo_group().is_none());
        let replacement = pcb.ensure_sem_undo_group(&ipc_ns).unwrap();
        assert!(!Arc::ptr_eq(&replacement, &old_group));
        assert_eq!(old_group.task_owners_for_test(), 0);
        assert_eq!(old_group.replay_count_for_test(), 1);
    }

    #[test]
    fn unshare_newipc_detaches_once_when_sysvsem_is_also_present() {
        let pcb = test_pcb();
        let old_nsproxy = pcb.nsproxy();
        let old_group = pcb.ensure_sem_undo_group(&old_nsproxy.ipc_ns).unwrap();
        let mut new_inner = old_nsproxy.clone_inner();
        new_inner.ipc_ns = old_nsproxy.ipc_ns.copy_ipc_ns(
            &CloneFlags::CLONE_NEWIPC,
            old_nsproxy.ipc_ns.user_ns.clone(),
        );
        let new_ipc_ns = new_inner.ipc_ns.clone();
        let fs_refs = lock_fs_refs_copy();

        let prepared = prepare_unshare_install(
            &pcb,
            CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_SYSVSEM,
            None,
            Some(Arc::new(new_inner)),
            None,
            &fs_refs,
        )
        .unwrap();
        prepared.commit(&pcb, &fs_refs).unwrap();

        assert!(pcb.sem_undo_group().is_none());
        assert!(Arc::ptr_eq(&pcb.nsproxy().ipc_ns, &new_ipc_ns));
        assert_eq!(old_group.task_owners_for_test(), 0);
        assert_eq!(old_group.replay_count_for_test(), 1);
    }
}
