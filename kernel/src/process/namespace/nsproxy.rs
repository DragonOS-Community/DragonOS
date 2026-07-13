use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};
use system_error::SystemError;

use crate::{
    filesystem::{fs::FsStruct, vfs::IndexNode},
    process::{
        cred::Cred,
        fork::CloneFlags,
        namespace::{
            cgroup_namespace::{CgroupNamespace, INIT_CGROUP_NAMESPACE},
            mnt::{root_mnt_namespace, MntNamespace},
            net_namespace::{NetNamespace, INIT_NET_NAMESPACE},
            uts_namespace::{UtsNamespace, INIT_UTS_NAMESPACE},
        },
        ProcessControlBlock, ProcessManager,
    },
};
use core::{fmt::Debug, intrinsics::likely};

use super::ipc_namespace::{IpcNamespace, INIT_IPC_NAMESPACE};
use super::{pid_namespace::PidNamespace, user_namespace::UserNamespace, NamespaceType};

int_like!(NamespaceId, AtomicNamespaceId, usize, AtomicUsize);
// ============================================================================
// Namespace ID Allocator
// ============================================================================

/// Global namespace inode number counter.
/// Namespace IDs start from 1 (0 means invalid/uninitialized).
/// This provides a stable, unique identifier for each namespace instance,
static NEXT_NS_INO: AtomicNamespaceId = AtomicNamespaceId::new(NamespaceId(1));

/// Allocate a new unique namespace id.
/// This ID remains stable throughout the namespace's lifetime and is used
/// for /proc/.../ns/ files (e.g., "ipc:[4026531839]").
pub fn alloc_ns_id() -> NamespaceId {
    NEXT_NS_INO.fetch_add(NamespaceId(1), Ordering::Relaxed)
}

/// A structure containing references to all per-process namespaces (filesystem/mount, UTS, network, etc.).
///
/// The PID namespace here is specifically for child processes (accessed via `task_active_pid_ns`).
///
/// Namespace references are counted by the number of nsproxies pointing to them, not by the number of tasks.
///
/// The nsproxy is shared by tasks that share all namespaces. It will be copied when any namespace is cloned or unshared.
/// 注意，user_ns 存储在cred,不存储在nsproxy
#[derive(Clone)]
pub struct NsProxy {
    /// PID namespace（用于子进程）
    pub pid_ns_for_children: Arc<PidNamespace>,
    /// mount namespace（挂载命名空间）
    pub mnt_ns: Arc<MntNamespace>,
    pub uts_ns: Arc<UtsNamespace>,
    /// ipc namespace（SysV IPC、POSIX mqueue 等）
    pub ipc_ns: Arc<IpcNamespace>,
    /// 网络命名空间
    pub net_ns: Arc<NetNamespace>,
    /// cgroup 命名空间
    pub cgroup_ns: Arc<CgroupNamespace>,
    // 注意，user_ns 存储在cred,不存储在nsproxy

    // 其他namespace（为未来扩展预留）
    // pub time_ns: Option<Arc<TimeNamespace>>,
}

impl Debug for NsProxy {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("NsProxy").finish()
    }
}

impl NsProxy {
    /// 创建root namespace代理
    pub fn new_root() -> Arc<Self> {
        let root_pid_ns = super::pid_namespace::INIT_PID_NAMESPACE.clone();
        let root_mnt_ns = root_mnt_namespace();
        let root_net_ns = INIT_NET_NAMESPACE.clone();
        let root_uts_ns = INIT_UTS_NAMESPACE.clone();
        let root_ipc_ns = INIT_IPC_NAMESPACE.clone();
        let root_cgroup_ns = INIT_CGROUP_NAMESPACE.clone();
        Arc::new(Self {
            pid_ns_for_children: root_pid_ns,
            mnt_ns: root_mnt_ns,
            net_ns: root_net_ns,
            uts_ns: root_uts_ns,
            ipc_ns: root_ipc_ns,
            cgroup_ns: root_cgroup_ns,
        })
    }

    /// 获取子进程的PID namespace
    pub fn pid_namespace_for_children(&self) -> &Arc<PidNamespace> {
        &self.pid_ns_for_children
    }

    /// 获取mount namespace
    pub fn mnt_namespace(&self) -> &Arc<MntNamespace> {
        &self.mnt_ns
    }

    /// 获取 net namespace
    pub fn net_namespace(&self) -> &Arc<NetNamespace> {
        &self.net_ns
    }

    pub fn clone_inner(&self) -> Self {
        Self {
            pid_ns_for_children: self.pid_ns_for_children.clone(),
            mnt_ns: self.mnt_ns.clone(),
            net_ns: self.net_ns.clone(),
            uts_ns: self.uts_ns.clone(),
            ipc_ns: self.ipc_ns.clone(),
            cgroup_ns: self.cgroup_ns.clone(),
        }
    }
}

impl ProcessManager {
    /// 拷贝namespace
    ///
    /// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/nsproxy.c?r=&mo=3770&fi=151#151
    #[inline(never)]
    pub fn copy_namespaces(
        clone_flags: &CloneFlags,
        _parent_pcb: &Arc<ProcessControlBlock>,
        child_pcb: &Arc<ProcessControlBlock>,
    ) -> Result<(), SystemError> {
        // log::debug!(
        //     "copy_namespaces: clone_flags={:?}, parent pid={}, child pid={}, child name={}",
        //     clone_flags,
        //     parent_pcb.raw_pid(),
        //     child_pcb.raw_pid(),
        //     child_pcb.basic().name()
        // );
        if likely(!clone_flags.intersects(
            CloneFlags::CLONE_NEWNS
                | CloneFlags::CLONE_NEWUTS
                | CloneFlags::CLONE_NEWIPC
                | CloneFlags::CLONE_NEWPID
                | CloneFlags::CLONE_NEWNET
                | CloneFlags::CLONE_NEWCGROUP
                | CloneFlags::CLONE_NEWTIME,
        )) && clone_flags.contains(CloneFlags::CLONE_VM)
        // || likely(parent_nsproxy.time_ns_for_children() == parent_nsproxy.time_ns())
        {
            // 由于在创建pcb的时候已经默认继承了parent的nsproxy，所以这里不需要做任何操作
            return Ok(());
        }

        let user_ns = if clone_flags.contains(CloneFlags::CLONE_NEWUSER) {
            let mut new_cred = (*child_pcb.cred()).clone();
            let new_user_ns =
                crate::process::namespace::user_namespace::UserNamespace::create_user_ns(
                    &new_cred,
                )?;
            crate::process::cred::set_cred_user_ns(&mut new_cred, new_user_ns.clone());
            child_pcb.set_cred(Cred::new_arc(new_cred))?;
            new_user_ns
        } else {
            child_pcb.cred().user_ns.clone()
        };

        /*
         * CLONE_NEWIPC must detach from the undolist: after switching
         * to a new ipc namespace, the semaphore arrays from the old
         * namespace are unreachable.  In clone parlance, CLONE_SYSVSEM
         * means share undolist with parent, so we must forbid using
         * it along with CLONE_NEWIPC.
         */

        if *clone_flags & (CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_SYSVSEM)
            == (CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_SYSVSEM)
        {
            return Err(SystemError::EINVAL);
        }
        let child_fs = child_pcb.fs_struct();
        let old_fs_root = child_fs.root();
        let old_fs_pwd = child_fs.pwd();
        let new_ns = create_new_namespaces(clone_flags, child_pcb, user_ns)?;
        let rebound_fs = if clone_flags.contains(CloneFlags::CLONE_NEWNS) {
            Some((
                new_ns.mnt_ns.project_copy_source_inode(&old_fs_root)?,
                new_ns.mnt_ns.project_copy_source_inode(&old_fs_pwd)?,
            ))
        } else {
            None
        };

        // All fallible mount/fs_struct projection completed before either
        // object is published to the child PCB.
        child_pcb.set_nsproxy(new_ns);
        if let Some((new_root, new_pwd)) = rebound_fs {
            child_fs.set_root(new_root);
            child_fs.set_pwd(new_pwd);
        }

        Ok(())
    }
}

/// 创建新的namespace代理及其所有关联的命名空间。
///
/// 返回新创建的nsproxy。调用者需要负责正确的加锁并将其附加到进程上。
///
/// 参考 https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/nsproxy.c?r=&mo=3770&fi=151#67
pub(super) fn create_new_namespaces(
    clone_flags: &CloneFlags,
    pcb: &Arc<ProcessControlBlock>,
    user_ns: Arc<UserNamespace>,
) -> Result<Arc<NsProxy>, SystemError> {
    let nsproxy = pcb.nsproxy();
    let pid_ns_for_children = nsproxy
        .pid_ns_for_children
        .copy_pid_ns(clone_flags, user_ns.clone())?;

    let mnt_ns = nsproxy.mnt_ns.copy_mnt_ns(clone_flags, user_ns.clone())?;
    let net_ns = nsproxy.net_ns.copy_net_ns(clone_flags, user_ns.clone())?;

    let uts_ns = nsproxy.uts_ns.copy_uts_ns(clone_flags, user_ns.clone())?;
    let ipc_ns = nsproxy.ipc_ns.copy_ipc_ns(clone_flags, user_ns.clone());
    let cgroup_ns = nsproxy
        .cgroup_ns
        .copy_cgroup_ns(clone_flags, user_ns.clone())?;

    let result = NsProxy {
        pid_ns_for_children,
        mnt_ns,
        net_ns,
        uts_ns,
        ipc_ns,
        cgroup_ns,
    };

    let result = Arc::new(result);
    return Ok(result);
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/include/linux/ns_common.h#9
/// 融合了 NamespaceBase 的公共字段
#[derive(Debug, Clone)]
pub struct NsCommon {
    /// 层级（root = 0）
    pub level: u32,
    /// 种类
    ty: NamespaceType,
    /// Namespace的唯一标识符（inode number），用于/proc/.../ns/文件
    /// 这个ID在namespace创建时分配，在整个namespace生命周期内保持不变
    /// 类似于Linux内核中的ns_common.inum字段
    pub nsid: NamespaceId,
}

impl NsCommon {
    /// Create a new NsCommon with an automatically allocated inode number.
    /// This is the preferred way to create NsCommon for new namespaces.
    pub fn new(level: u32, ty: NamespaceType) -> Self {
        Self {
            level,
            ty,
            nsid: alloc_ns_id(),
        }
    }

    pub fn level(&self) -> u32 {
        self.level
    }

    pub fn ty(&self) -> NamespaceType {
        self.ty
    }
}

impl Default for NsCommon {
    fn default() -> Self {
        // Note: This should rarely be used. Prefer explicit creation with new().
        Self::new(0, NamespaceType::Pid)
    }
}

/// https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/nsproxy.c?fi=exec_task_namespaces#259
pub fn exec_task_namespaces() -> Result<(), SystemError> {
    let tsk = ProcessManager::current_pcb();
    let user_ns = tsk.cred().user_ns.clone();
    let new_nsproxy = create_new_namespaces(&CloneFlags::empty(), &tsk, user_ns)?;
    // todo: time_ns的逻辑
    switch_task_namespaces(&tsk, new_nsproxy)?;

    return Ok(());
}

pub fn switch_task_namespaces(
    tsk: &Arc<ProcessControlBlock>,
    new_nsproxy: Arc<NsProxy>,
) -> Result<(), SystemError> {
    // Check sharing before taking our temporary Arc below.  Counting after
    // cloning the Arc would mistake this function's own reference for a
    // CLONE_FS peer and reject an otherwise private fs_struct.
    let fs_is_shared = tsk.fs_struct_is_shared();
    let fs = tsk.fs_struct();
    switch_task_namespaces_inner(tsk, &fs, new_nsproxy, fs_is_shared, false)
}

pub(crate) fn switch_task_namespaces_with_fs(
    tsk: &Arc<ProcessControlBlock>,
    fs: &Arc<FsStruct>,
    new_nsproxy: Arc<NsProxy>,
) -> Result<(), SystemError> {
    // unshare(CLONE_NEWNS) passes either a freshly copied fs_struct or the
    // task's proven-private one, so there is no CLONE_FS peer to reject.
    switch_task_namespaces_inner(tsk, fs, new_nsproxy, false, true)
}

fn switch_task_namespaces_inner(
    tsk: &Arc<ProcessControlBlock>,
    fs: &Arc<FsStruct>,
    new_nsproxy: Arc<NsProxy>,
    fs_is_shared: bool,
    project_copy_source: bool,
) -> Result<(), SystemError> {
    if !Arc::ptr_eq(tsk.nsproxy().mnt_namespace(), &new_nsproxy.mnt_ns) {
        if fs_is_shared {
            return Err(SystemError::EINVAL);
        }
        if project_copy_source {
            prepare_fs_for_new_mntns(fs, &new_nsproxy.mnt_ns)?;
        } else {
            // setns installs the target namespace root as both root and cwd;
            // it must not preserve paths merely because the target happens to
            // have been cloned from the current namespace.
            let root = new_nsproxy.mnt_ns.root_inode();
            fs.set_root(root.clone());
            fs.set_pwd(root);
        }
    }

    tsk.set_nsproxy(new_nsproxy);
    Ok(())
}

pub(crate) fn prepare_fs_for_new_mntns(
    fs: &Arc<FsStruct>,
    new_mntns: &Arc<MntNamespace>,
) -> Result<(), SystemError> {
    let (new_root, new_pwd) = resolve_fs_paths_for_new_mntns(fs, new_mntns)?;
    fs.set_root(new_root);
    fs.set_pwd(new_pwd);
    Ok(())
}

type ReboundFsPaths = (Arc<dyn IndexNode>, Arc<dyn IndexNode>);

fn resolve_fs_paths_for_new_mntns(
    fs: &Arc<FsStruct>,
    new_mntns: &Arc<MntNamespace>,
) -> Result<ReboundFsPaths, SystemError> {
    let old_root = fs.root();
    let old_pwd = fs.pwd();
    if let (Ok(new_root), Ok(new_pwd)) = (
        new_mntns.project_copy_source_inode(&old_root),
        new_mntns.project_copy_source_inode(&old_pwd),
    ) {
        return Ok((new_root, new_pwd));
    }

    let namespace_root = new_mntns.root_inode();
    Ok((namespace_root.clone(), namespace_root))
}
