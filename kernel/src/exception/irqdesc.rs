use core::{any::Any, fmt::Debug};

use alloc::{
    collections::BTreeMap,
    string::String,
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::CurrentIrqArch,
    driver::base::{
        device::DeviceId,
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        rwlock::{RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    process::ProcessControlBlock,
};

use super::{
    dummychip::no_irq_chip,
    handle::bad_irq_handler,
    irqdata::{IrqCommonData, IrqData, IrqStatus},
    sysfs::IrqKObjType,
    HardwareIrqNumber, InterruptArch, IrqNumber,
};

/// 中断流处理程序
pub trait IrqFlowHandler: Debug + Send + Sync {
    fn handle(&self, irq_desc: &Arc<IrqDesc>);
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdesc.h#55
#[derive(Debug)]
pub struct IrqDesc {
    inner: SpinLock<InnerIrqDesc>,

    handler: SpinLock<Option<&'static dyn IrqFlowHandler>>,

    kobj_state: LockedKObjectState,
}

impl IrqDesc {
    #[inline(never)]
    pub fn new(irq: IrqNumber, name: Option<String>, irqd_flags: IrqStatus) -> Arc<Self> {
        // 初始化的过程参考 https://code.dragonos.org.cn/xref/linux-6.1.9/kernel/irq/irqdesc.c#392
        let common_data = Arc::new(IrqCommonData::new());
        let irq_data = Arc::new(IrqData::new(
            irq,
            HardwareIrqNumber::new(irq.data()),
            common_data.clone(),
            no_irq_chip(),
        ));

        irq_data.irqd_set(IrqStatus::IRQD_IRQ_DISABLED);
        common_data.irqd_set(IrqStatus::IRQD_IRQ_MASKED);

        let irq_desc = IrqDesc {
            inner: SpinLock::new(InnerIrqDesc {
                common_data,
                irq_data,
                actions: Vec::new(),
                name,
                parent_irq: None,
                depth: 1,
                wake_depth: 0,
                kern_inode: None,
                kset: None,
                parent_kobj: None,
            }),
            handler: SpinLock::new(None),
            kobj_state: LockedKObjectState::new(Some(KObjectState::INITIALIZED)),
        };

        irq_desc.set_handler(bad_irq_handler());
        irq_desc.inner().irq_data.irqd_set(irqd_flags);

        return Arc::new(irq_desc);
    }

    pub fn set_handler(&self, handler: &'static dyn IrqFlowHandler) {
        let mut guard = self.handler.lock_irqsave();
        *guard = Some(handler);
    }

    fn inner(&self) -> SpinLockGuard<InnerIrqDesc> {
        self.inner.lock_irqsave()
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqDesc {
    /// per irq and chip data passed down to chip functions
    common_data: Arc<IrqCommonData>,
    irq_data: Arc<IrqData>,
    actions: Vec<Arc<IrqAction>>,
    name: Option<String>,
    parent_irq: Option<IrqNumber>,
    /// nested irq disables
    depth: u32,
    /// nested wake enables
    wake_depth: u32,

    kern_inode: Option<Arc<KernFSInode>>,
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
}

impl KObject for IrqDesc {
    fn as_any_ref(&self) -> &dyn Any {
        self
    }

    fn set_inode(&self, inode: Option<Arc<KernFSInode>>) {
        self.inner().kern_inode = inode;
    }

    fn inode(&self) -> Option<Arc<KernFSInode>> {
        self.inner().kern_inode.clone()
    }

    fn parent(&self) -> Option<Weak<dyn KObject>> {
        self.inner().parent_kobj.clone()
    }

    fn set_parent(&self, parent: Option<Weak<dyn KObject>>) {
        self.inner().parent_kobj = parent;
    }

    fn kset(&self) -> Option<Arc<KSet>> {
        self.inner().kset.clone()
    }

    fn set_kset(&self, kset: Option<Arc<KSet>>) {
        self.inner().kset = kset;
    }

    fn kobj_type(&self) -> Option<&'static dyn KObjType> {
        Some(&IrqKObjType)
    }

    fn set_kobj_type(&self, _ktype: Option<&'static dyn KObjType>) {}

    fn name(&self) -> String {
        self.inner().name.clone().unwrap_or_else(|| format!(""))
    }

    fn set_name(&self, _name: String) {}

    fn kobj_state(&self) -> RwLockReadGuard<KObjectState> {
        self.kobj_state.read()
    }

    fn kobj_state_mut(&self) -> RwLockWriteGuard<KObjectState> {
        self.kobj_state.write()
    }

    fn set_kobj_state(&self, state: KObjectState) {
        *self.kobj_state_mut() = state;
    }
}

/// 每个中断的响应动作的描述符
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/interrupt.h#118
#[allow(dead_code)]
#[derive(Debug)]
pub struct IrqAction {
    inner: SpinLock<InnerIrqAction>,
}

impl IrqAction {
    #[allow(dead_code)]
    pub fn new(
        irq: IrqNumber,
        name: String,
        handler: Option<&'static dyn IrqFlowHandler>,
    ) -> Arc<Self> {
        let action = IrqAction {
            inner: SpinLock::new(InnerIrqAction {
                dev_id: None,
                handler,
                thread_fn: None,
                thread: None,
                secondary: None,
                irq,
                flags: IrqHandleFlags::empty(),
                name,
            }),
        };

        return Arc::new(action);
    }
}

#[allow(dead_code)]
#[derive(Debug)]
struct InnerIrqAction {
    /// cookie to identify the device
    dev_id: Option<DeviceId>,
    /// 中断处理程序
    handler: Option<&'static dyn IrqFlowHandler>,
    /// interrupt handler function for threaded interrupts
    thread_fn: Option<&'static dyn IrqFlowHandler>,
    /// thread pointer for threaded interrupts
    thread: Option<Arc<ProcessControlBlock>>,
    /// pointer to secondary irqaction (force threading)
    secondary: Option<Arc<IrqAction>>,
    /// 中断号
    irq: IrqNumber,
    flags: IrqHandleFlags,
    /// name of the device
    name: String,
}

// 定义IrqFlags位标志
bitflags! {
    /// 这些标志仅由内核在中断处理例程中使用。
    pub struct IrqHandleFlags: u32 {
        /// IRQF_SHARED - 允许多个设备共享中断
        const IRQF_SHARED = 0x00000080;
        /// IRQF_PROBE_SHARED - 当预期出现共享不匹配时，由调用者设置
        const IRQF_PROBE_SHARED = 0x00000100;
        /// IRQF_TIMER - 标记此中断为定时器中断
        const __IRQF_TIMER = 0x00000200;
        /// IRQF_PERCPU - 中断是每个CPU的
        const IRQF_PERCPU = 0x00000400;
        /// IRQF_NOBALANCING - 将此中断从中断平衡中排除
        const IRQF_NOBALANCING = 0x00000800;
        /// IRQF_IRQPOLL - 中断用于轮询（出于性能原因，只有在共享中断中首次注册的中断会被考虑）
        const IRQF_IRQPOLL = 0x00001000;
        /// IRQF_ONESHOT - 在硬中断处理程序完成后，不会重新启用中断。由需要在运行线程处理程序之前保持中断线路禁用的线程中断使用。
        const IRQF_ONESHOT = 0x00002000;
        /// IRQF_NO_SUSPEND - 在挂起期间不禁用此IRQ。不能保证此中断会从挂起状态唤醒系统。
        const IRQF_NO_SUSPEND = 0x00004000;
        /// IRQF_FORCE_RESUME - 即使设置了IRQF_NO_SUSPEND，也强制在恢复时启用它
        const IRQF_FORCE_RESUME = 0x00008000;
        /// IRQF_NO_THREAD - 中断不能被线程化
        const IRQF_NO_THREAD = 0x00010000;
        /// IRQF_EARLY_RESUME - 在syscore而不是在设备恢复时间早期恢复IRQ。
        const IRQF_EARLY_RESUME = 0x00020000;
        /// IRQF_COND_SUSPEND - 如果IRQ与NO_SUSPEND用户共享，则在挂起中断后执行此中断处理程序。对于系统唤醒设备用户，需要在他们的中断处理程序中实现唤醒检测。
        const IRQF_COND_SUSPEND = 0x00040000;
        /// IRQF_NO_AUTOEN - 当用户请求时，不会自动启用IRQ或NMI。用户稍后会通过enable_irq()或enable_nmi()显式启用它。
        const IRQF_NO_AUTOEN = 0x00080000;
        /// IRQF_NO_DEBUG - 从IPI和类似处理程序的逃逸检测中排除，取决于IRQF_PERCPU。
        const IRQF_NO_DEBUG = 0x00100000;
        const IRQF_TIMER = Self::__IRQF_TIMER.bits | Self::IRQF_NO_SUSPEND.bits | Self::IRQF_NO_THREAD.bits;
    }
}

#[inline(never)]
pub(super) fn early_irq_init() -> Result<(), SystemError> {
    let irqcnt = CurrentIrqArch::probe_total_irq_num();
    let mut manager = IrqDescManager::new();
    for i in 0..irqcnt {
        let irq_desc = IrqDesc::new(IrqNumber::new(i), None, IrqStatus::empty());
        manager.insert(IrqNumber::new(i), irq_desc);
    }

    return CurrentIrqArch::arch_early_irq_init();
}

pub(super) struct IrqDescManager {
    irq_descs: BTreeMap<IrqNumber, Arc<IrqDesc>>,
}

impl IrqDescManager {
    fn new() -> Self {
        IrqDescManager {
            irq_descs: BTreeMap::new(),
        }
    }

    /// 查找中断描述符
    #[allow(dead_code)]
    pub fn lookup(&self, irq: IrqNumber) -> Option<Arc<IrqDesc>> {
        self.irq_descs.get(&irq).map(|desc| desc.clone())
    }

    fn insert(&mut self, irq: IrqNumber, desc: Arc<IrqDesc>) {
        self.irq_descs.insert(irq, desc);
    }
}
