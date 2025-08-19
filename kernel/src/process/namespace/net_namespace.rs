use crate::arch::CurrentIrqArch;
use crate::driver::net::loopback::{generate_loopback_iface_default, LoopbackInterface};
use crate::exception::InterruptArch;
use crate::init::initcall::INITCALL_SUBSYS;
use crate::libs::rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use crate::net::routing::Router;
use crate::process::fork::CloneFlags;
use crate::process::kthread::{KernelThreadClosure, KernelThreadMechanism};
use crate::process::namespace::{NamespaceOps, NamespaceType};
use crate::process::{ProcessControlBlock, ProcessManager};
use crate::sched::{schedule, SchedMode};
use crate::{
    driver::net::Iface,
    libs::spinlock::SpinLock,
    process::namespace::{nsproxy::NsCommon, user_namespace::UserNamespace},
};
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::sync::{Arc, Weak};
use system_error::SystemError;
use unified_init::macros::unified_init;

lazy_static! {
    /// # 所有网络设备，进程，socket的初始网络命名空间
    pub static ref INIT_NET_NAMESPACE: Arc<NetNamespace> = NetNamespace::new_root();
}

#[unified_init(INITCALL_SUBSYS)]
pub fn root_net_namespace_thread_init() -> Result<(), SystemError> {
    // 创建root网络命名空间的轮询线程
    let pcb =
        NetNamespace::create_polling_thread(INIT_NET_NAMESPACE.clone(), "root_netns".to_string());
    INIT_NET_NAMESPACE.set_poll_thread(pcb);
    Ok(())
}

#[derive(Debug)]
pub struct NetNamespace {
    ns_common: NsCommon,
    self_ref: Weak<NetNamespace>,
    _user_ns: Arc<UserNamespace>,
    inner: RwLock<InnerNetNamespace>,
    /// # 负责当前网络命名空间网卡轮询的线程
    net_poll_thread: SpinLock<Option<Arc<ProcessControlBlock>>>,
    /// # 当前网络命名空间下所有网络接口的列表
    /// 这个列表在中断上下文会使用到，因此需要irqsave
    /// 没有放在InnerNetNamespace里面，独立出来，方便管理
    device_list: RwLock<BTreeMap<usize, Arc<dyn Iface>>>,
}

#[derive(Debug)]
pub struct InnerNetNamespace {
    router: Arc<Router>,
    /// 当前网络命名空间的loopback网卡
    loopback_iface: Option<Arc<LoopbackInterface>>,
    default_iface: Option<Arc<dyn Iface>>,
}

impl InnerNetNamespace {
    pub fn router(&self) -> &Arc<Router> {
        &self.router
    }

    pub fn loopback_iface(&self) -> Option<Arc<LoopbackInterface>> {
        self.loopback_iface.clone()
    }
}

impl NetNamespace {
    pub fn new_root() -> Arc<Self> {
        let inner = InnerNetNamespace {
            router: Arc::new(Router::new("root_netns_router".to_string())),
            loopback_iface: None,
            default_iface: None,
        };

        let netns = Arc::new_cyclic(|self_ref| Self {
            ns_common: NsCommon::new(0, NamespaceType::Net),
            self_ref: self_ref.clone(),
            _user_ns: super::user_namespace::INIT_USER_NAMESPACE.clone(),
            inner: RwLock::new(inner),
            net_poll_thread: SpinLock::new(None),
            device_list: RwLock::new(BTreeMap::new()),
        });

        // Self::create_polling_thread(netns.clone(), "netns_root".to_string());
        log::info!("Initialized root net namespace");
        netns
    }

    pub fn new_empty(user_ns: Arc<UserNamespace>) -> Result<Arc<Self>, SystemError> {
        // 这里获取当前进程的pid，只是为了给后面创建的路由以及线程做唯一标识，没有其他意义
        let pid = ProcessManager::current_pid().0;
        let loopback = generate_loopback_iface_default();

        let inner = InnerNetNamespace {
            router: Arc::new(Router::new(format!("netns_router_{}", pid))),
            loopback_iface: Some(loopback),
            default_iface: None,
        };

        let netns = Arc::new_cyclic(|self_ref| Self {
            ns_common: NsCommon::new(0, NamespaceType::Net),
            self_ref: self_ref.clone(),
            _user_ns: user_ns,
            inner: RwLock::new(inner),
            net_poll_thread: SpinLock::new(None),
            device_list: RwLock::new(BTreeMap::new()),
        });
        Self::create_polling_thread(netns.clone(), format!("netns_{}", pid));

        Ok(netns)
    }

    pub(super) fn copy_net_ns(
        &self,
        clone_flags: &CloneFlags,
        user_ns: Arc<UserNamespace>,
    ) -> Result<Arc<Self>, SystemError> {
        if !clone_flags.contains(CloneFlags::CLONE_NEWNET) {
            return Ok(self.self_ref.upgrade().unwrap());
        }

        Self::new_empty(user_ns)
    }

    pub fn device_list_write(&self) -> RwLockWriteGuard<'_, BTreeMap<usize, Arc<dyn Iface>>> {
        self.device_list.write_irqsave()
    }

    pub fn device_list(&self) -> RwLockReadGuard<'_, BTreeMap<usize, Arc<dyn Iface>>> {
        self.device_list.read_irqsave()
    }

    pub fn inner(&self) -> RwLockReadGuard<'_, InnerNetNamespace> {
        self.inner.read()
    }

    pub fn inner_mut(&self) -> RwLockWriteGuard<'_, InnerNetNamespace> {
        self.inner.write()
    }

    pub fn set_loopback_iface(&self, loopback: Arc<LoopbackInterface>) {
        self.inner_mut().loopback_iface = Some(loopback);
    }

    pub fn loopback_iface(&self) -> Option<Arc<LoopbackInterface>> {
        self.inner().loopback_iface()
    }

    pub fn set_default_iface(&self, iface: Arc<dyn Iface>) {
        self.inner_mut().default_iface = Some(iface);
    }

    pub fn default_iface(&self) -> Option<Arc<dyn Iface>> {
        self.inner().default_iface.clone()
    }

    pub fn add_device(&self, device: Arc<dyn Iface>) {
        device.set_net_namespace(self.self_ref.upgrade().unwrap());

        self.device_list
            .write_irqsave()
            .insert(device.nic_id(), device);

        log::info!(
            "Network device added to namespace count: {:?}",
            self.device_list.read_irqsave().len()
        );
    }

    pub fn remove_device(&self, nic_id: &usize) {
        self.device_list.write_irqsave().remove(nic_id);
    }

    /// # 拉起网络命名空间的轮询线程
    pub fn wakeup_poll_thread(&self) {
        if self.net_poll_thread.lock().is_none() {
            return;
        }
        // log::info!("wakeup net_poll thread for namespace");
        let _ = ProcessManager::wakeup(self.net_poll_thread.lock().as_ref().unwrap());
    }

    /// # 网络命名空间的轮询线程
    /// 该线程会轮询当前命名空间下的所有网络接口
    /// 并调用它们的poll方法
    /// 注意： 此方法仅可在初始化当前net namespace时创建进程使用
    fn polling(&self) {
        log::info!("net_poll thread started for namespace");
        loop {
            for (_, iface) in self.device_list.read_irqsave().iter() {
                iface.poll();
            }
            let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
            ProcessManager::mark_sleep(true)
                .expect("clocksource_watchdog_kthread:mark sleep failed");
            drop(irq_guard);
            schedule(SchedMode::SM_NONE);
        }
    }

    fn create_polling_thread(netns: Arc<Self>, name: String) -> Arc<ProcessControlBlock> {
        let pcb = {
            let closure: Box<dyn Fn() -> i32 + Send + Sync> = Box::new(move || {
                netns.polling();
                0
            });
            KernelThreadClosure::EmptyClosure((closure, ()))
        };

        let pcb = KernelThreadMechanism::create_and_run(pcb, name)
            .ok_or("")
            .expect("create net_poll thread for net namespace failed");
        log::info!("net_poll thread created for namespace");
        pcb
    }

    /// # 设置网络命名空间的轮询线程
    /// 这个方法仅可在初始化网络命名空间时调用
    fn set_poll_thread(&self, pcb: Arc<ProcessControlBlock>) {
        let mut lock = self.net_poll_thread.lock();
        *lock = Some(pcb);
    }
}

impl NamespaceOps for NetNamespace {
    fn ns_common(&self) -> &NsCommon {
        &self.ns_common
    }
}

impl ProcessManager {
    pub fn current_netns() -> Arc<NetNamespace> {
        Self::current_pcb().nsproxy.read().net_ns.clone()
    }
}
