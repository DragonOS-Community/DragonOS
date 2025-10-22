use alloc::sync::Arc;
use system_error::SystemError;

use crate::process::{
    fork::CloneFlags,
    namespace::{
        mnt::{root_mnt_namespace, MntNamespace},
        net_namespace::{NetNamespace, INIT_NET_NAMESPACE},
        uts_namespace::{UtsNamespace, INIT_UTS_NAMESPACE},
    },
    ProcessControlBlock, ProcessManager,
};
use core::{fmt::Debug, intrinsics::likely};

use super::ipc_namespace::{IpcNamespace, INIT_IPC_NAMESPACE};
use super::{pid_namespace::PidNamespace, user_namespace::UserNamespace, NamespaceType};

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
    // 注意，user_ns 存储在cred,不存储在nsproxy

    // 其他namespace（为未来扩展预留）
    // pub cgroup_ns: Option<Arc<CgroupNamespace>>,
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
        Arc::new(Self {
            pid_ns_for_children: root_pid_ns,
            mnt_ns: root_mnt_ns,
            net_ns: root_net_ns,
            uts_ns: root_uts_ns,
            ipc_ns: root_ipc_ns,
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

        // todo: 这里要添加一个对user_namespace的处理
        // https://code.dragonos.org.cn/xref/linux-6.6.21/kernel/nsproxy.c?r=&mo=3770&fi=151#165

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
        let user_ns = child_pcb.cred().user_ns.clone();
        let new_ns = create_new_namespaces(clone_flags, child_pcb, user_ns)?;
        // 设置新的nsproxy

        child_pcb.set_nsproxy(new_ns);

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
    let result = NsProxy {
        pid_ns_for_children,
        mnt_ns,
        net_ns,
        uts_ns,
        ipc_ns,
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
}

impl NsCommon {
    pub fn new(level: u32, ty: NamespaceType) -> Self {
        Self { level, ty }
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
        Self::new(0, NamespaceType::Pid) // 默认值，实际使用时应该明确指定
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
    tsk.set_nsproxy(new_nsproxy);
    Ok(())
}
