use alloc::sync::Arc;

use system_error::SystemError;

use crate::{
    filesystem::vfs::file::{FilePrivateData, NamespaceFilePrivateData},
    process::{
        cred::{ns_capable, CAPFlags, Cred},
        fork::CloneFlags,
        pid::PidType,
        ProcessManager,
    },
};

use super::nsproxy::{switch_task_namespaces, NsProxy};

fn can_setns_cgroup(target: &crate::process::namespace::cgroup_namespace::CgroupNamespace) -> bool {
    let current = ProcessManager::current_pcb();
    let cred = current.cred();
    if !cred.has_cap_sys_admin() {
        return false;
    }

    let current_user_ns = cred.user_ns.clone();
    let target_user_ns = target.user_ns().clone();
    current_user_ns.is_ancestor_of(&target_user_ns)
}

fn flags_match(flags: CloneFlags, expected: CloneFlags) -> bool {
    flags.is_empty() || flags == expected
}

fn can_setns_target_userns(
    target_user_ns: &Arc<crate::process::namespace::user_namespace::UserNamespace>,
) -> bool {
    let current = ProcessManager::current_pcb();
    let cred = current.cred();
    cred.has_cap_sys_admin() && cred.user_ns.is_ancestor_of(target_user_ns)
}

fn can_access_pidfd_setns_target(target: &Arc<crate::process::ProcessControlBlock>) -> bool {
    let current = ProcessManager::current_pcb();
    if Arc::ptr_eq(&current, target) {
        return true;
    }

    let current_cred = current.cred();
    if current_cred.has_capability(CAPFlags::CAP_SYS_PTRACE) {
        return true;
    }

    let target_cred = target.cred();
    current_cred.uid == target_cred.euid
        && current_cred.uid == target_cred.suid
        && current_cred.uid == target_cred.uid
        && current_cred.gid == target_cred.egid
        && current_cred.gid == target_cred.sgid
        && current_cred.gid == target_cred.gid
}

fn nsfd_target_userns(
    ns_fd: &NamespaceFilePrivateData,
) -> Option<Arc<crate::process::namespace::user_namespace::UserNamespace>> {
    match ns_fd {
        NamespaceFilePrivateData::Ipc(ns) => Some(ns.user_ns.clone()),
        NamespaceFilePrivateData::Uts(ns) => Some(ns.user_ns().clone()),
        NamespaceFilePrivateData::Mnt(ns) => Some(ns.user_ns().clone()),
        NamespaceFilePrivateData::Net(ns) => Some(ns.user_ns().clone()),
        NamespaceFilePrivateData::Pid(ns) | NamespaceFilePrivateData::PidForChildren(ns) => {
            Some(ns.user_ns().clone())
        }
        NamespaceFilePrivateData::Cgroup(ns) => Some(ns.user_ns().clone()),
        NamespaceFilePrivateData::User(_) => None,
    }
}

/// 内核态 setns 实现（当前仅支持 pidfd + namespace flag 形式）
///
/// - `fd`：必须是通过 `pidfd_open` 或 `clone(CLONE_PIDFD)` 获得的 pidfd
/// - `nstype`：命名空间 flag 组合。pidfd 路径当前仅支持 CLONE_NEWNS/CLONE_NEWUTS/
///   CLONE_NEWIPC/CLONE_NEWNET/CLONE_NEWPID/CLONE_NEWCGROUP；namespace fd 路径额外支持 CLONE_NEWUSER
///
/// 语义（与 Linux setns(pidfd, flags) 对齐的子集）：
/// - 针对指定 flag，从目标任务的 `NsProxy` 中拷贝对应 namespace 引用，
///   在当前任务上构造新的 `NsProxy` 并通过 `switch_task_namespaces` 原子替换
/// - CLONE_NEWPID 仅影响 `pid_ns_for_children`（与 DragonOS/ Linux 一致）
/// - user namespace 当前仅支持通过 `/proc/<pid>/ns/user` 这类 namespace fd 进入
#[inline(never)]
pub fn ksys_setns(fd: i32, nstype: i32) -> Result<(), SystemError> {
    // 1. 解析并校验 flag
    let flags = CloneFlags::from_bits(nstype as u64).ok_or(SystemError::EINVAL)?;

    const SETNS_VALID_FLAGS: CloneFlags = CloneFlags::from_bits_truncate(
        CloneFlags::CLONE_NEWNS.bits()
            | CloneFlags::CLONE_NEWUTS.bits()
            | CloneFlags::CLONE_NEWIPC.bits()
            | CloneFlags::CLONE_NEWNET.bits()
            | CloneFlags::CLONE_NEWUSER.bits()
            | CloneFlags::CLONE_NEWPID.bits()
            | CloneFlags::CLONE_NEWCGROUP.bits(),
    );

    // 不能包含未支持的位；对 pidfd 路径，后续会额外要求非空
    if flags.intersects(!SETNS_VALID_FLAGS) {
        return Err(SystemError::EINVAL);
    }

    // 2. 解析 fd，当前仅支持 pidfd
    let current = ProcessManager::current_pcb();
    let fd_table = current.fd_table();
    let file = fd_table
        .read()
        .get_file_by_fd(fd)
        .ok_or(SystemError::EBADF)?;

    // 3. 根据 fd 类型决定 setns 模式：namespace fd / pidfd
    let ns_fd = {
        let pdata = file.private_data.lock();
        match &*pdata {
            FilePrivateData::Namespace(n) => Some(n.clone()),
            _ => None,
        }
    };

    // pidfd 路径：flags 必须非空。fd 存在但不是 pidfd 时不能返回 EBADF；
    // setns 语义要求这种类型不匹配返回 EINVAL。
    if ns_fd.is_none() {
        let Some(target_pid) = file.try_pidfd_target().ok() else {
            return Err(SystemError::EINVAL);
        };
        if flags.is_empty() {
            return Err(SystemError::EINVAL);
        }
        if flags.contains(CloneFlags::CLONE_NEWUSER) {
            return Err(SystemError::EINVAL);
        }

        let target = target_pid.task(PidType::TGID).ok_or(SystemError::ESRCH)?;
        if !can_access_pidfd_setns_target(&target) {
            return Err(SystemError::EPERM);
        }
        if !can_setns_target_userns(&target.cred().user_ns) {
            return Err(SystemError::EPERM);
        }

        // 基于当前任务的 NsProxy 构造新的代理，并按 flag 覆盖为目标的各命名空间
        let cur_nsproxy = current.nsproxy();
        let target_nsproxy = target.nsproxy();

        let mut new_inner: NsProxy = cur_nsproxy.clone_inner();

        if flags.contains(CloneFlags::CLONE_NEWNS) {
            new_inner.mnt_ns = target_nsproxy.mnt_ns.clone();
        }
        if flags.contains(CloneFlags::CLONE_NEWUTS) {
            new_inner.uts_ns = target_nsproxy.uts_ns.clone();
        }
        if flags.contains(CloneFlags::CLONE_NEWIPC) {
            new_inner.ipc_ns = target_nsproxy.ipc_ns.clone();
        }
        if flags.contains(CloneFlags::CLONE_NEWNET) {
            new_inner.net_ns = target_nsproxy.net_ns.clone();
        }
        if flags.contains(CloneFlags::CLONE_NEWPID) {
            // 与 Linux 语义一致：仅影响子进程的 PID namespace
            new_inner.pid_ns_for_children = target.active_pid_ns();
        }
        if flags.contains(CloneFlags::CLONE_NEWCGROUP) {
            if !can_setns_cgroup(&target_nsproxy.cgroup_ns) {
                return Err(SystemError::EPERM);
            }
            new_inner.cgroup_ns = target_nsproxy.cgroup_ns.clone();
        }

        let new_nsproxy = Arc::new(new_inner);
        switch_task_namespaces(&current, new_nsproxy)?;
        return Ok(());
    }

    // namespace fd 路径：fd 指向 /proc/thread-self/ns/* 打开的命名空间
    let Some(ns_fd) = ns_fd else {
        // 既不是 pidfd，也不是 namespace fd
        return Err(SystemError::EINVAL);
    };

    // 如果 flags 为空，则允许 “按 fd 类型推断”；否则必须与 fd 的 namespace 类型匹配。
    let mut new_inner: NsProxy = current.nsproxy().clone_inner();
    if let Some(target_user_ns) = nsfd_target_userns(&ns_fd) {
        if !can_setns_target_userns(&target_user_ns) {
            return Err(SystemError::EPERM);
        }
    }

    match ns_fd {
        NamespaceFilePrivateData::Ipc(ns) => {
            if !flags_match(flags, CloneFlags::CLONE_NEWIPC) {
                return Err(SystemError::EINVAL);
            }
            new_inner.ipc_ns = ns;
        }
        NamespaceFilePrivateData::Uts(ns) => {
            if !flags_match(flags, CloneFlags::CLONE_NEWUTS) {
                return Err(SystemError::EINVAL);
            }
            new_inner.uts_ns = ns;
        }
        NamespaceFilePrivateData::Mnt(ns) => {
            if !flags_match(flags, CloneFlags::CLONE_NEWNS) {
                return Err(SystemError::EINVAL);
            }
            new_inner.mnt_ns = ns;
        }
        NamespaceFilePrivateData::Net(ns) => {
            if !flags_match(flags, CloneFlags::CLONE_NEWNET) {
                return Err(SystemError::EINVAL);
            }
            new_inner.net_ns = ns;
        }
        NamespaceFilePrivateData::Pid(ns) | NamespaceFilePrivateData::PidForChildren(ns) => {
            if !flags_match(flags, CloneFlags::CLONE_NEWPID) {
                return Err(SystemError::EINVAL);
            }
            // 仅影响子进程 PID namespace，保持与 Linux 语义一致
            new_inner.pid_ns_for_children = ns;
        }
        NamespaceFilePrivateData::User(ns) => {
            if !flags.is_empty() && !flags.contains(CloneFlags::CLONE_NEWUSER) {
                return Err(SystemError::EINVAL);
            }
            userns_install(&current, ns)?;
            return Ok(());
        }
        NamespaceFilePrivateData::Cgroup(ns) => {
            if !flags_match(flags, CloneFlags::CLONE_NEWCGROUP) {
                return Err(SystemError::EINVAL);
            }
            if !can_setns_cgroup(&ns) {
                return Err(SystemError::EPERM);
            }
            new_inner.cgroup_ns = ns;
        }
    }

    let new_nsproxy = Arc::new(new_inner);

    // 5. 原子切换当前任务的 namespace 代理
    switch_task_namespaces(&current, new_nsproxy)?;

    Ok(())
}

/// 安装（切换）user namespace（对应 Linux userns_install）
fn userns_install(
    current: &Arc<crate::process::ProcessControlBlock>,
    user_ns: Arc<super::user_namespace::UserNamespace>,
) -> Result<(), SystemError> {
    // 1. 不能与当前 ns 相同（防止重复获得能力）
    if Arc::ptr_eq(&current.cred().user_ns, &user_ns) {
        return Err(SystemError::EINVAL);
    }

    // 2. 不能共享线程组
    if !current.threads_read_irqsave().thread_group_empty() {
        return Err(SystemError::EINVAL);
    }

    // 3. 不能共享 fs_struct
    if current.fs_struct_is_shared() {
        return Err(SystemError::EINVAL);
    }

    // 4. 需要 CAP_SYS_ADMIN 在目标 ns
    if !ns_capable(&user_ns, CAPFlags::CAP_SYS_ADMIN) {
        return Err(SystemError::EPERM);
    }

    // 5. 先准备新的 cred，全部校验通过后再提交
    let mut new_cred = (*current.cred()).clone();
    crate::process::cred::set_cred_user_ns(&mut new_cred, user_ns);
    current.set_cred(Cred::new_arc(new_cred))?;

    Ok(())
}
