use alloc::sync::Arc;

use system_error::SystemError;

use crate::{
    filesystem::vfs::file::{FilePrivateData, NamespaceFilePrivateData},
    process::{fork::CloneFlags, ProcessManager, RawPid},
};

use super::nsproxy::{switch_task_namespaces, NsProxy};

/// 内核态 setns 实现（当前仅支持 pidfd + namespace flag 形式）
///
/// - `fd`：必须是通过 `pidfd_open` 或 `clone(CLONE_PIDFD)` 获得的 pidfd
/// - `nstype`：命名空间 flag 组合，仅允许 CLONE_NEWNS/CLONE_NEWUTS/CLONE_NEWIPC/
///   CLONE_NEWNET/CLONE_NEWPID，且不能为空
///
/// 语义（与 Linux setns(pidfd, flags) 对齐的子集）：
/// - 针对指定 flag，从目标任务的 `NsProxy` 中拷贝对应 namespace 引用，
///   在当前任务上构造新的 `NsProxy` 并通过 `switch_task_namespaces` 原子替换
/// - CLONE_NEWPID 仅影响 `pid_ns_for_children`（与 DragonOS/ Linux 一致）
/// - 不支持 USER/CGROUP/TIME namespace 以及 `/proc/<pid>/ns/*` 路径
#[inline(never)]
pub fn ksys_setns(fd: i32, nstype: i32) -> Result<(), SystemError> {
    // 1. 解析并校验 flag
    let flags = CloneFlags::from_bits(nstype as u64).ok_or(SystemError::EINVAL)?;

    const SETNS_VALID_FLAGS: CloneFlags = CloneFlags::from_bits_truncate(
        CloneFlags::CLONE_NEWNS.bits()
            | CloneFlags::CLONE_NEWUTS.bits()
            | CloneFlags::CLONE_NEWIPC.bits()
            | CloneFlags::CLONE_NEWNET.bits()
            | CloneFlags::CLONE_NEWPID.bits(),
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

    // 3. 根据 fd 类型决定 setns 模式：pidfd / namespace fd
    let (pidfd_pid, ns_fd) = {
        let pdata = file.private_data.lock();
        match &*pdata {
            FilePrivateData::Pid(p) => (Some(p.pid()), None),
            FilePrivateData::Namespace(n) => (None, Some(n.clone())),
            _ => (None, None),
        }
    };

    // pidfd 路径：flags 必须非空
    if let Some(pid) = pidfd_pid {
        if pid < 0 {
            return Err(SystemError::EBADF);
        }
        if flags.is_empty() {
            return Err(SystemError::EINVAL);
        }

        let target_pid = RawPid::new(pid as usize);
        let target = ProcessManager::find_task_by_vpid(target_pid).ok_or(SystemError::ESRCH)?;

        // TODO: 权限模型（ptrace_may_access / user_ns 能力检查）

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
            new_inner.pid_ns_for_children = target_nsproxy.pid_ns_for_children.clone();
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

    match ns_fd {
        NamespaceFilePrivateData::Ipc(ns) => {
            if !flags.is_empty() && !flags.contains(CloneFlags::CLONE_NEWIPC) {
                return Err(SystemError::EINVAL);
            }
            new_inner.ipc_ns = ns;
        }
        NamespaceFilePrivateData::Uts(ns) => {
            if !flags.is_empty() && !flags.contains(CloneFlags::CLONE_NEWUTS) {
                return Err(SystemError::EINVAL);
            }
            new_inner.uts_ns = ns;
        }
        NamespaceFilePrivateData::Mnt(ns) => {
            if !flags.is_empty() && !flags.contains(CloneFlags::CLONE_NEWNS) {
                return Err(SystemError::EINVAL);
            }
            new_inner.mnt_ns = ns;
        }
        NamespaceFilePrivateData::Net(ns) => {
            if !flags.is_empty() && !flags.contains(CloneFlags::CLONE_NEWNET) {
                return Err(SystemError::EINVAL);
            }
            new_inner.net_ns = ns;
        }
        NamespaceFilePrivateData::Pid(ns) | NamespaceFilePrivateData::PidForChildren(ns) => {
            if !flags.is_empty() && !flags.contains(CloneFlags::CLONE_NEWPID) {
                return Err(SystemError::EINVAL);
            }
            // 仅影响子进程 PID namespace，保持与 Linux 语义一致
            new_inner.pid_ns_for_children = ns;
        }
        NamespaceFilePrivateData::User(_ns) => {
            // 暂未实现 user namespace 切换
            return Err(SystemError::EINVAL);
        }
    }

    let new_nsproxy = Arc::new(new_inner);

    // 5. 原子切换当前任务的 namespace 代理
    switch_task_namespaces(&current, new_nsproxy)?;

    Ok(())
}
