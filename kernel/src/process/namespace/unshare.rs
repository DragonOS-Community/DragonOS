use alloc::sync::Arc;

use system_error::SystemError;

use crate::{
    filesystem::fs::FsStruct,
    process::{
        cred::{ns_capable, CAPFlags, Cred},
        fork::CloneFlags,
        namespace::nsproxy::{
            create_new_namespaces, switch_task_namespaces, switch_task_namespaces_with_fs, NsProxy,
        },
        ProcessManager,
    },
};

/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/fork.c#3385
pub fn ksys_unshare(flags: CloneFlags) -> Result<(), SystemError> {
    let flags = normalize_unshare_flags(flags);

    // 检查 unshare 标志位
    check_unshare_flags(flags)?;

    let current_pcb = ProcessManager::current_pcb();
    let mut new_cred = unshare_user_cred(flags, &current_pcb)?;
    let new_fs = unshare_fs_struct(flags, &current_pcb)?;
    let new_nsproxy =
        unshare_nsproxy_namespaces(flags, &current_pcb, new_cred.as_ref(), new_fs.as_ref())?;

    if let Some(new_nsproxy) = new_nsproxy {
        if let Some(new_fs) = new_fs.as_ref() {
            switch_task_namespaces_with_fs(&current_pcb, new_fs, new_nsproxy)?;
        } else {
            switch_task_namespaces(&current_pcb, new_nsproxy)?;
        }
    }

    if let Some(new_fs) = new_fs {
        current_pcb.set_fs_struct(new_fs);
    }

    if let Some(new_cred) = new_cred.take() {
        current_pcb.set_cred(Cred::new_arc(new_cred))?;
    }

    // TODO: 处理其他命名空间的 unshare 操作
    // CLONE_FS, CLONE_FILES, CLONE_SIGHAND, CLONE_VM, CLONE_THREAD, CLONE_SYSVSEM,
    // CLONE_NEWUTS, CLONE_NEWIPC, CLONE_NEWUSER, CLONE_NEWNET, CLONE_NEWCGROUP, CLONE_NEWTIME

    Ok(())
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
