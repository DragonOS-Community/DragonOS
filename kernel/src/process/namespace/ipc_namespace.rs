//! # IPC Namespace 实现
//! 
//! 本模块实现了 IPC namespace，用于隔离进程间通信资源，包括：
//! - POSIX 共享内存 (shm_open/shm_unlink)
//! - System V IPC (共享内存、信号量、消息队列)
//! - POSIX 消息队列
//! - 命名管道

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicUsize, Ordering};

use system_error::SystemError;
use crate::ipc::shm::ShmManager;
use crate::libs::spinlock::SpinLock;
use crate::mm::shmem::TmpFS;
use crate::process::namespace::{
    nsproxy::NsCommon, user_namespace::UserNamespace, NamespaceOps, NamespaceType,
};
use crate::process::ProcessManager;

/// IPC namespace 全局ID分配器
static IPC_NS_ID_ALLOCATOR: AtomicUsize = AtomicUsize::new(1);

// 根 IPC 命名空间
lazy_static::lazy_static! {
    pub static ref INIT_IPC_NAMESPACE: Arc<IpcNamespace> = IpcNamespace::new_root();
}

/// DragonOS 的 IPC 命名空间
#[derive(Debug)]
pub struct IpcNamespace {
    ns_common: NsCommon,
    self_ref: Weak<IpcNamespace>,
    /// 关联的 user namespace (权限判断使用)
    pub user_ns: Arc<UserNamespace>,
    /// namespace ID，用于标识不同的 IPC namespace
    ns_id: usize,
    /// /dev/shm tmpfs 实例
    dev_shm_tmpfs: Option<Arc<TmpFS>>,

    /// SysV SHM 管理器（阶段一：仅支持 per-ns shm）
    pub shm: SpinLock<ShmManager>,
    // System V IPC 资源（预留）
    // sysv_shm: SpinLock<SysVShmManager>,
    // sysv_sem: SpinLock<SysVSemManager>, 
    // sysv_msg: SpinLock<SysVMsgManager>,
    // POSIX 消息队列（预留）
    // mq: SpinLock<PosixMqManager>,
}

impl NamespaceOps for IpcNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl IpcNamespace {
    /// 创建新的 IPC namespace
    pub fn new() -> Arc<Self> {
        let ns_id = IPC_NS_ID_ALLOCATOR.fetch_add(1, Ordering::SeqCst);
        
        Arc::new_cyclic(|weak_self| Self {
            ns_common: NsCommon::new(0, NamespaceType::Ipc),
            self_ref: weak_self.clone(),
            user_ns: crate::process::namespace::user_namespace::INIT_USER_NAMESPACE.clone(),
            ns_id,
            dev_shm_tmpfs: None,
            shm: SpinLock::new(ShmManager::new()),
        })
    }

    /// 获取 namespace ID
    pub fn ns_id(&self) -> usize {
        self.ns_id
    }

    /// 设置 /dev/shm tmpfs 实例
    pub fn set_dev_shm_tmpfs(&mut self, tmpfs: Arc<TmpFS>) {
        self.dev_shm_tmpfs = Some(tmpfs);
    }

    /// 获取 /dev/shm tmpfs 实例
    pub fn dev_shm_tmpfs(&self) -> Option<&Arc<TmpFS>> {
        self.dev_shm_tmpfs.as_ref()
    }

    /// 克隆 IPC namespace（用于 unshare 或 clone）
    pub fn clone_ns(&self) -> Arc<Self> {
        // 创建新的 IPC namespace，不继承任何 IPC 资源
        // 这符合 Linux 的行为：新的 IPC namespace 从空白状态开始
        Self::new()
        }

    fn new_root() -> Arc<Self> {
        Arc::new_cyclic(|weak_self| Self {
            ns_common: NsCommon::new(0, NamespaceType::Ipc),
            self_ref: weak_self.clone(),
            user_ns: crate::process::namespace::user_namespace::INIT_USER_NAMESPACE.clone(),
            ns_id: 0,
            dev_shm_tmpfs: None,
            shm: SpinLock::new(ShmManager::new()),
        })
    }

    /// 拷贝 IPC namespace（参考 Linux kernel 的 copy_ipcs 实现）
    /// 
    /// 如果 clone_flags 包含 CLONE_NEWIPC，则创建新的 IPC namespace；
    /// 否则，返回当前 namespace 的引用计数副本
    pub fn copy_ipc_ns(
        &self,
        clone_flags: &crate::process::fork::CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<IpcNamespace>, SystemError> {
        use crate::process::fork::CloneFlags;
        if clone_flags.contains(CloneFlags::CLONE_NEWIPC) {
            // 创建新的 IPC 命名空间，SHM 空间独立
            let ns_id = IPC_NS_ID_ALLOCATOR.fetch_add(1, Ordering::SeqCst);
            Ok(Arc::new_cyclic(|weak_self| IpcNamespace {
                ns_common: NsCommon::new(self.ns_common.level + 1, NamespaceType::Ipc),
                self_ref: weak_self.clone(),
                user_ns,
                ns_id,
                dev_shm_tmpfs: None,
                shm: SpinLock::new(ShmManager::new()),
            }))
        } else {
            // 共享当前的 IPC namespace，增加引用计数
            Ok(self.self_ref.upgrade().unwrap())
        }
    }

    /// 获取 namespace 统计信息
    pub fn get_stats(&self) -> IpcNamespaceStats {
        IpcNamespaceStats {
            namespace_id: self.ns_id,
            posix_shm_objects: 0, // 这个需要从实际的管理器中获取
            active_posix_shm_objects: 0,
            // 其他 IPC 资源统计可以在这里添加
        }
    }
}

/// IPC namespace 统计信息
#[derive(Debug, Clone)]
pub struct IpcNamespaceStats {
    /// namespace ID
    pub namespace_id: usize,
    /// POSIX 共享内存对象总数
    pub posix_shm_objects: usize,
    /// 活跃的 POSIX 共享内存对象数
    pub active_posix_shm_objects: usize,
}

impl ProcessManager {
    /// 获取当前进程的 IPC namespace
    /// 
    /// 这个函数模仿 Linux 内核的行为：
    /// - 在进程管理器初始化之前，总是返回根 IPC namespace
    /// - 在进程管理器初始化之后，返回当前进程的 IPC namespace
    /// 
    /// 这样确保了在系统启动的任何阶段都能安全地获取 IPC namespace
    pub fn current_ipcns() -> Arc<IpcNamespace> {
        if Self::initialized() {
            ProcessManager::current_pcb().nsproxy.read().ipc_ns.clone()
        } else {
            INIT_IPC_NAMESPACE.clone()
        }
    }
}

/// 获取根 IPC namespace
/// 
/// 这个函数在系统启动的任何阶段都可以安全调用，
/// 它会返回静态初始化的根 IPC namespace
pub fn root_ipc_namespace() -> Arc<IpcNamespace> {
    INIT_IPC_NAMESPACE.clone()
}

/// 早期初始化 IPC namespace 子系统
/// 
/// 这个函数在系统启动早期调用，只进行最基本的初始化，
/// 不依赖进程管理器或其他复杂的子系统
pub fn init_ipc_namespace() -> Result<(), SystemError> {
    // 确保根 IPC namespace 被创建
    let _root_ns = INIT_IPC_NAMESPACE.clone();
    
    log::warn!("IPC namespace subsystem early initialized"); // 使用 warn 级别确保可见
    Ok(())
}

/// 完整初始化 IPC namespace 子系统
/// 
/// 这个函数在进程管理器初始化完成后调用，负责初始化所有 IPC 机制：
/// - POSIX 共享内存
/// - System V IPC（未来）
/// - POSIX 消息队列（未来）
/// 
/// 模仿 Linux 内核中 IPC namespace 作为容器管理所有 IPC 资源的模式
pub fn init_ipc_namespace_full() -> Result<(), SystemError> {
    // 注意：基础 IPC namespace 已在系统早期初始化中完成
    // 这里只进行需要进程上下文的后期初始化
    
    // 1. 初始化 POSIX 共享内存子系统
    crate::ipc::posix_shm::init_posix_shm()?;
    
    // 2. 设置 /dev/shm 挂载点（需要进程上下文）
    if let Err(e) = crate::ipc::posix_shm::setup_dev_shm() {
        log::warn!("Failed to setup /dev/shm: {:?}, continuing without it", e);
    }
    
    // 4. 未来可以在这里添加其他 IPC 机制的初始化
    // init_sysv_ipc()?;
    // init_posix_mq()?;
    
    log::warn!("IPC namespace subsystem fully initialized"); // 使用 warn 级别确保可见
    Ok(())
}
