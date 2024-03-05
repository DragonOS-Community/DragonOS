use core::{
    any::Any,
    fmt::Debug,
    sync::atomic::{AtomicI64, Ordering},
};

use alloc::{
    collections::{btree_map, BTreeMap},
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use system_error::SystemError;

use crate::{
    arch::{interrupt::TrapFrame, CurrentIrqArch},
    driver::base::{
        device::DeviceId,
        kobject::{KObjType, KObject, KObjectState, LockedKObjectState},
        kset::KSet,
    },
    filesystem::kernfs::KernFSInode,
    libs::{
        cpumask::CpuMask,
        mutex::{Mutex, MutexGuard},
        rwlock::{RwLock, RwLockReadGuard, RwLockWriteGuard},
        spinlock::{SpinLock, SpinLockGuard},
    },
    mm::percpu::PerCpuVar,
    process::ProcessControlBlock,
    sched::completion::Completion,
    smp::cpu::smp_cpu_manager,
};

use super::{
    dummychip::no_irq_chip,
    handle::bad_irq_handler,
    irqchip::IrqChip,
    irqdata::{IrqCommonData, IrqData, IrqHandlerData, IrqLineStatus, IrqStatus},
    sysfs::{irq_sysfs_del, IrqKObjType},
    HardwareIrqNumber, InterruptArch, IrqNumber,
};

/// 中断流处理程序
pub trait IrqFlowHandler: Debug + Send + Sync + Any {
    fn handle(&self, irq_desc: &Arc<IrqDesc>, trap_frame: &mut TrapFrame);
}

/// 中断处理程序
pub trait IrqHandler: Debug + Send + Sync + Any {
    fn handle(
        &self,
        irq: IrqNumber,
        static_data: Option<&dyn IrqHandlerData>,
        dynamic_data: Option<Arc<dyn IrqHandlerData>>,
    ) -> Result<IrqReturn, SystemError>;
}

/// 中断处理函数返回值
///
/// 用于指示中断处理函数是否处理了中断
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqReturn {
    /// 中断未被处理
    NotHandled,
    /// 中断已被处理
    Handled,
    /// 中断已被处理，并且需要唤醒中断线程
    WakeThread,
}

/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/irqdesc.h#55
#[derive(Debug)]
pub struct IrqDesc {
    inner: SpinLock<InnerIrqDesc>,

    handler: RwLock<Option<&'static dyn IrqFlowHandler>>,
    /// 一个用于串行化 request_irq()和free_irq() 的互斥锁
    request_mutex: Mutex<()>,
    kobj_state: LockedKObjectState,
    /// 当前描述符内正在运行的中断线程数
    threads_active: AtomicI64,
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
        common_data.insert_status(IrqStatus::IRQD_IRQ_MASKED);

        let irq_desc = IrqDesc {
            inner: SpinLock::new(InnerIrqDesc {
                percpu_affinity: None,
                percpu_enabled: None,
                common_data,
                irq_data,
                desc_internal_state: IrqDescState::empty(),
                line_status: IrqLineStatus::empty(),
                actions: Vec::new(),
                name,
                parent_irq: None,
                depth: 1,
                wake_depth: 0,
                kern_inode: None,
                kset: None,
                parent_kobj: None,
            }),
            request_mutex: Mutex::new(()),
            handler: RwLock::new(None),
            kobj_state: LockedKObjectState::new(Some(KObjectState::INITIALIZED)),
            threads_active: AtomicI64::new(0),
        };

        irq_desc.set_handler(bad_irq_handler());
        irq_desc.inner().irq_data.irqd_set(irqd_flags);

        return Arc::new(irq_desc);
    }

    /// 返回当前活跃的中断线程数量
    #[allow(dead_code)]
    pub fn threads_active(&self) -> i64 {
        self.threads_active.load(Ordering::SeqCst)
    }

    /// 增加当前活跃的中断线程数量, 返回增加前的值
    pub fn inc_threads_active(&self) -> i64 {
        self.threads_active.fetch_add(1, Ordering::SeqCst)
    }

    /// 减少当前活跃的中断线程数量, 返回减少前的值
    #[allow(dead_code)]
    pub fn dec_threads_active(&self) -> i64 {
        self.threads_active.fetch_sub(1, Ordering::SeqCst)
    }

    pub fn set_handler(&self, handler: &'static dyn IrqFlowHandler) {
        self.chip_bus_lock();
        let mut guard = self.handler.write_irqsave();
        *guard = Some(handler);
        self.chip_bus_sync_unlock();
    }

    /// 设置中断处理程序（不对desc->inner）
    ///
    ///
    /// ## Safety
    ///
    /// 需要保证irq_data和chip是当前irqdesc的
    pub fn set_handler_no_lock_inner(
        &self,
        handler: &'static dyn IrqFlowHandler,
        irq_data: &Arc<IrqData>,
        chip: &Arc<dyn IrqChip>,
    ) {
        chip.irq_bus_lock(irq_data).ok();
        let mut guard = self.handler.write_irqsave();
        *guard = Some(handler);
        chip.irq_bus_sync_unlock(irq_data).ok();
    }

    pub fn handler(&self) -> Option<&'static dyn IrqFlowHandler> {
        let guard = self.handler.read_irqsave();
        *guard
    }

    pub fn inner(&self) -> SpinLockGuard<InnerIrqDesc> {
        self.inner.lock_irqsave()
    }

    pub fn actions(&self) -> Vec<Arc<IrqAction>> {
        self.inner().actions.clone()
    }

    /// 对中断请求过程加锁
    pub fn request_mutex_lock(&self) -> MutexGuard<()> {
        self.request_mutex.lock()
    }

    pub fn irq(&self) -> IrqNumber {
        self.inner().irq_data.irq()
    }

    pub fn hardware_irq(&self) -> HardwareIrqNumber {
        self.inner().irq_data.hardware_irq()
    }

    pub fn irq_data(&self) -> Arc<IrqData> {
        self.inner().irq_data.clone()
    }

    /// 标记当前irq描述符已经被添加到sysfs
    pub fn mark_in_sysfs(&self) {
        self.inner()
            .desc_internal_state
            .insert(IrqDescState::IRQS_SYSFS);
    }

    pub fn mark_not_in_sysfs(&self) {
        self.inner()
            .desc_internal_state
            .remove(IrqDescState::IRQS_SYSFS);
    }

    /// 判断当前描述符是否已经添加到了sysfs
    pub fn in_sysfs(&self) -> bool {
        self.inner()
            .desc_internal_state
            .contains(IrqDescState::IRQS_SYSFS)
    }

    pub fn name(&self) -> Option<String> {
        self.inner().name.clone()
    }

    pub fn can_request(&self) -> bool {
        self.inner().can_request()
    }

    #[allow(dead_code)]
    pub fn set_norequest(&self) {
        self.inner().set_norequest();
    }

    #[allow(dead_code)]
    pub fn clear_norequest(&self) {
        self.inner().clear_norequest();
    }

    pub fn nested_thread(&self) -> bool {
        self.inner().nested_thread()
    }

    /// 中断是否可以线程化
    pub fn can_thread(&self) -> bool {
        !self
            .inner()
            .line_status
            .contains(IrqLineStatus::IRQ_NOTHREAD)
    }

    pub fn chip_bus_lock(&self) {
        let irq_data = self.inner().irq_data.clone();
        irq_data
            .chip_info_read_irqsave()
            .chip()
            .irq_bus_lock(&irq_data)
            .ok();
    }

    /// 同步释放低速总线锁
    ///
    /// ## 锁
    ///
    /// 进入此函数时，必须持有低速总线锁，并且desc的inner锁和irqdata的inner锁
    ///     必须已经释放。否则将死锁。
    pub fn chip_bus_sync_unlock(&self) {
        let irq_data = self.inner().irq_data.clone();
        irq_data
            .chip_info_write_irqsave()
            .chip()
            .irq_bus_sync_unlock(&irq_data)
            .ok();
    }

    pub fn set_percpu_devid_flags(&self) {
        self.modify_status(
            IrqLineStatus::empty(),
            IrqLineStatus::IRQ_NOAUTOEN
                | IrqLineStatus::IRQ_PER_CPU
                | IrqLineStatus::IRQ_NOTHREAD
                | IrqLineStatus::IRQ_NOPROBE
                | IrqLineStatus::IRQ_PER_CPU_DEVID,
        );
    }

    pub fn modify_status(&self, clear: IrqLineStatus, set: IrqLineStatus) {
        let mut desc_guard = self.inner();
        desc_guard.line_status.remove(clear);
        desc_guard.line_status.insert(set);

        let mut trigger = desc_guard.common_data().trigger_type();

        desc_guard.common_data().clear_status(
            IrqStatus::IRQD_NO_BALANCING
                | IrqStatus::IRQD_PER_CPU
                | IrqStatus::IRQD_TRIGGER_MASK
                | IrqStatus::IRQD_LEVEL
                | IrqStatus::IRQD_MOVE_PCNTXT,
        );

        if desc_guard
            .line_status
            .contains(IrqLineStatus::IRQ_NO_BALANCING)
        {
            desc_guard
                .common_data()
                .insert_status(IrqStatus::IRQD_NO_BALANCING);
        }

        if desc_guard.line_status.contains(IrqLineStatus::IRQ_PER_CPU) {
            desc_guard
                .common_data()
                .insert_status(IrqStatus::IRQD_PER_CPU);
        }

        if desc_guard
            .line_status
            .contains(IrqLineStatus::IRQ_MOVE_PCNTXT)
        {
            desc_guard
                .common_data()
                .insert_status(IrqStatus::IRQD_MOVE_PCNTXT);
        }

        if desc_guard.line_status.is_level_type() {
            desc_guard
                .common_data()
                .insert_status(IrqStatus::IRQD_LEVEL);
        }

        let tmp = desc_guard.line_status.trigger_type();

        if tmp != IrqLineStatus::IRQ_TYPE_NONE {
            trigger = tmp;
        }

        desc_guard.common_data().set_trigger_type(trigger);
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct InnerIrqDesc {
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
    desc_internal_state: IrqDescState,
    /// 中断线的状态
    line_status: IrqLineStatus,

    kern_inode: Option<Arc<KernFSInode>>,
    kset: Option<Arc<KSet>>,
    parent_kobj: Option<Weak<dyn KObject>>,
    /// per-cpu enabled mask
    percpu_enabled: Option<CpuMask>,
    /// per-cpu affinity
    percpu_affinity: Option<CpuMask>,
    // wait_for_threads: EventWaitQueue
}

impl InnerIrqDesc {
    pub fn name(&self) -> Option<&String> {
        self.name.as_ref()
    }

    #[allow(dead_code)]
    pub fn set_name(&mut self, name: Option<String>) {
        self.name = name;
    }

    pub fn can_request(&self) -> bool {
        !self.line_status.contains(IrqLineStatus::IRQ_NOREQUEST)
    }

    #[allow(dead_code)]
    pub fn set_norequest(&mut self) {
        self.line_status.insert(IrqLineStatus::IRQ_NOREQUEST);
    }

    #[allow(dead_code)]
    pub fn clear_norequest(&mut self) {
        self.line_status.remove(IrqLineStatus::IRQ_NOREQUEST);
    }

    #[allow(dead_code)]
    pub fn set_noprobe(&mut self) {
        self.line_status.insert(IrqLineStatus::IRQ_NOPROBE);
    }

    #[allow(dead_code)]
    pub fn clear_noprobe(&mut self) {
        self.line_status.remove(IrqLineStatus::IRQ_NOPROBE);
    }

    pub fn set_nothread(&mut self) {
        self.line_status.insert(IrqLineStatus::IRQ_NOTHREAD);
    }

    pub fn clear_nothread(&mut self) {
        self.line_status.remove(IrqLineStatus::IRQ_NOTHREAD);
    }

    pub fn nested_thread(&self) -> bool {
        self.line_status.contains(IrqLineStatus::IRQ_NESTED_THREAD)
    }

    pub fn line_status_set_per_cpu(&mut self) {
        self.line_status.insert(IrqLineStatus::IRQ_PER_CPU);
    }

    #[allow(dead_code)]
    pub fn line_status_clear_per_cpu(&mut self) {
        self.line_status.remove(IrqLineStatus::IRQ_PER_CPU);
    }

    #[allow(dead_code)]
    pub fn line_status(&self) -> &IrqLineStatus {
        &self.line_status
    }

    pub fn line_status_set_no_debug(&mut self) {
        self.line_status.insert(IrqLineStatus::IRQ_NO_BALANCING);
    }

    #[allow(dead_code)]
    pub fn line_status_clear_no_debug(&mut self) {
        self.line_status.remove(IrqLineStatus::IRQ_NO_BALANCING);
    }

    pub fn can_autoenable(&self) -> bool {
        !self.line_status.contains(IrqLineStatus::IRQ_NOAUTOEN)
    }

    pub fn can_thread(&self) -> bool {
        !self.line_status.contains(IrqLineStatus::IRQ_NOTHREAD)
    }

    /// 中断是否可以设置CPU亲和性
    pub fn can_set_affinity(&self) -> bool {
        if self.common_data.status().can_balance() == false
            || self
                .irq_data()
                .chip_info_read_irqsave()
                .chip()
                .can_set_affinity()
                == false
        {
            return false;
        }

        return true;
    }

    pub fn actions(&self) -> &Vec<Arc<IrqAction>> {
        &self.actions
    }

    pub fn add_action(&mut self, action: Arc<IrqAction>) {
        self.actions.push(action);
    }

    pub fn clear_actions(&mut self) {
        self.actions.clear();
    }

    pub fn remove_action(&mut self, action: &Arc<IrqAction>) {
        self.actions.retain(|a| !Arc::ptr_eq(a, action));
    }

    pub fn internal_state(&self) -> &IrqDescState {
        &self.desc_internal_state
    }

    pub(super) fn internal_state_mut(&mut self) -> &mut IrqDescState {
        &mut self.desc_internal_state
    }

    pub fn irq_data(&self) -> &Arc<IrqData> {
        &self.irq_data
    }

    pub fn common_data(&self) -> &Arc<IrqCommonData> {
        &self.common_data
    }

    pub fn depth(&self) -> u32 {
        self.depth
    }

    pub fn wake_depth(&self) -> u32 {
        self.wake_depth
    }

    pub fn set_depth(&mut self, depth: u32) {
        self.depth = depth;
    }

    pub fn set_trigger_type(&mut self, trigger: IrqLineStatus) {
        self.line_status.remove(IrqLineStatus::IRQ_TYPE_SENSE_MASK);
        self.line_status
            .insert(trigger & IrqLineStatus::IRQ_TYPE_SENSE_MASK);
    }

    pub fn clear_level(&mut self) {
        self.line_status.remove(IrqLineStatus::IRQ_LEVEL);
    }

    pub fn set_level(&mut self) {
        self.line_status.insert(IrqLineStatus::IRQ_LEVEL);
    }

    pub fn percpu_enabled(&self) -> &Option<CpuMask> {
        &self.percpu_enabled
    }

    pub fn percpu_enabled_mut(&mut self) -> &mut Option<CpuMask> {
        &mut self.percpu_enabled
    }

    pub fn percpu_affinity(&self) -> &Option<CpuMask> {
        &self.percpu_affinity
    }

    pub fn percpu_affinity_mut(&mut self) -> &mut Option<CpuMask> {
        &mut self.percpu_affinity
    }
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
        self.inner().irq_data.irq().data().to_string()
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

bitflags! {
    /// Bit masks for desc->desc_internal_state
    pub struct IrqDescState: u32 {
        /// autodetection in progress
        const IRQS_AUTODETECT = 0x00000001;
        /// was disabled due to spurious interrupt detection
        const IRQS_SPURIOUS_DISABLED = 0x00000002;
        /// polling in progress
        const IRQS_POLL_INPROGRESS = 0x00000008;
        /// irq is not unmasked in primary handler
        const IRQS_ONESHOT = 0x00000020;
        /// irq is replayed
        const IRQS_REPLAY = 0x00000040;
        /// irq is waiting
        const IRQS_WAITING = 0x00000080;
        /// irq is pending and replayed later
        const IRQS_PENDING = 0x00000200;
        /// irq is suspended
        const IRQS_SUSPENDED = 0x00000800;
        /// irq line is used to deliver NMIs
        const IRQS_NMI = 0x00002000;
        /// descriptor has been added to sysfs
        const IRQS_SYSFS = 0x00004000;
    }
}

/// 每个中断的响应动作的描述符
/// 参考 https://code.dragonos.org.cn/xref/linux-6.1.9/include/linux/interrupt.h#118
#[allow(dead_code)]
#[derive(Debug)]
pub struct IrqAction {
    inner: SpinLock<InnerIrqAction>,
    /// 用于等待线程被创建的完成量
    thread_completion: Completion,
}

impl IrqAction {
    #[allow(dead_code)]
    pub fn new(
        irq: IrqNumber,
        name: String,
        handler: Option<&'static dyn IrqHandler>,
        thread_fn: Option<&'static dyn IrqHandler>,
    ) -> Arc<Self> {
        let action: IrqAction = IrqAction {
            inner: SpinLock::new(InnerIrqAction {
                dev_id: None,
                per_cpu_dev_id: None,
                handler,
                thread_fn,
                thread: None,
                secondary: None,
                irq,
                flags: IrqHandleFlags::empty(),
                name,
                thread_flags: ThreadedHandlerFlags::empty(),
            }),
            thread_completion: Completion::new(),
        };

        return Arc::new(action);
    }

    pub fn inner(&self) -> SpinLockGuard<InnerIrqAction> {
        self.inner.lock_irqsave()
    }

    pub fn thread_completion(&self) -> &Completion {
        &self.thread_completion
    }
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct InnerIrqAction {
    /// cookie to identify the device
    dev_id: Option<Arc<DeviceId>>,
    /// cookie to identify the device (per cpu)
    per_cpu_dev_id: Option<PerCpuVar<Arc<DeviceId>>>,
    /// 中断处理程序
    handler: Option<&'static dyn IrqHandler>,
    /// interrupt handler function for threaded interrupts
    thread_fn: Option<&'static dyn IrqHandler>,
    /// thread pointer for threaded interrupts
    thread: Option<Arc<ProcessControlBlock>>,
    /// pointer to secondary irqaction (force threading)
    secondary: Option<Arc<IrqAction>>,
    /// 中断号
    irq: IrqNumber,
    flags: IrqHandleFlags,
    /// 中断线程的标志
    thread_flags: ThreadedHandlerFlags,
    /// name of the device
    name: String,
}

impl InnerIrqAction {
    pub fn dev_id(&self) -> &Option<Arc<DeviceId>> {
        &self.dev_id
    }

    pub fn dev_id_mut(&mut self) -> &mut Option<Arc<DeviceId>> {
        &mut self.dev_id
    }

    pub fn per_cpu_dev_id(&self) -> Option<&Arc<DeviceId>> {
        self.per_cpu_dev_id.as_ref().map(|v| v.get())
    }

    #[allow(dead_code)]
    pub fn per_cpu_dev_id_mut(&mut self) -> Option<&mut Arc<DeviceId>> {
        self.per_cpu_dev_id.as_mut().map(|v| v.get_mut())
    }

    pub fn handler(&self) -> Option<&'static dyn IrqHandler> {
        self.handler
    }

    pub fn set_handler(&mut self, handler: Option<&'static dyn IrqHandler>) {
        self.handler = handler;
    }

    pub fn thread_fn(&self) -> Option<&'static dyn IrqHandler> {
        self.thread_fn
    }

    pub fn thread(&self) -> Option<Arc<ProcessControlBlock>> {
        self.thread.clone()
    }

    pub fn set_thread(&mut self, thread: Option<Arc<ProcessControlBlock>>) {
        self.thread = thread;
    }

    #[allow(dead_code)]
    pub fn thread_flags(&self) -> &ThreadedHandlerFlags {
        &self.thread_flags
    }

    pub fn thread_flags_mut(&mut self) -> &mut ThreadedHandlerFlags {
        &mut self.thread_flags
    }

    pub fn secondary(&self) -> Option<Arc<IrqAction>> {
        self.secondary.clone()
    }

    #[allow(dead_code)]
    pub fn irq(&self) -> IrqNumber {
        self.irq
    }

    #[allow(dead_code)]
    pub fn set_irq(&mut self, irq: IrqNumber) {
        self.irq = irq;
    }

    pub fn flags(&self) -> &IrqHandleFlags {
        &self.flags
    }

    pub fn flags_mut(&mut self) -> &mut IrqHandleFlags {
        &mut self.flags
    }

    pub fn name(&self) -> &String {
        &self.name
    }
}

bitflags! {
    /// 这些标志由线程处理程序使用
    pub struct ThreadedHandlerFlags: u32 {
        /// IRQTF_RUNTHREAD - 表示应运行中断处理程序线程
        const IRQTF_RUNTHREAD = 1 << 0;
        /// IRQTF_WARNED - 已打印警告 "IRQ_WAKE_THREAD w/o thread_fn"
        const IRQTF_WARNED = 1 << 1;
        /// IRQTF_AFFINITY - 请求irq线程调整亲和性
        const IRQTF_AFFINITY = 1 << 2;
        /// IRQTF_FORCED_THREAD - irq操作被强制线程化
        const IRQTF_FORCED_THREAD = 1 << 3;
        /// IRQTF_READY - 表示irq线程已准备就绪
        const IRQTF_READY = 1 << 4;
    }
}

/// Implements the `ThreadedHandlerFlags` structure.
impl ThreadedHandlerFlags {
    /// 在 `ThreadedHandlerFlags` 结构中测试并设置特定的位。
    ///
    /// # 参数
    ///
    /// * `bit` - 要测试并设置的位。
    ///
    /// # 返回
    ///
    /// 如果操作前该位已被设置，则返回 `true`，否则返回 `false`。
    pub fn test_and_set_bit(&mut self, bit: ThreadedHandlerFlags) -> bool {
        let res = (self.bits & bit.bits) != 0;
        self.bits |= bit.bits;
        return res;
    }
}

// 定义IrqFlags位标志
bitflags! {
    /// 这些标志仅由内核在中断处理例程中使用。
    pub struct IrqHandleFlags: u32 {

        const IRQF_TRIGGER_NONE = IrqLineStatus::IRQ_TYPE_NONE.bits();
        const IRQF_TRIGGER_RISING = IrqLineStatus::IRQ_TYPE_EDGE_RISING.bits();
        const IRQF_TRIGGER_FALLING = IrqLineStatus::IRQ_TYPE_EDGE_FALLING.bits();
        const IRQF_TRIGGER_HIGH = IrqLineStatus::IRQ_TYPE_LEVEL_HIGH.bits();
        const IRQF_TRIGGER_LOW = IrqLineStatus::IRQ_TYPE_LEVEL_LOW.bits();
        const IRQF_TRIGGER_MASK = Self::IRQF_TRIGGER_HIGH.bits | Self::IRQF_TRIGGER_LOW.bits | Self::IRQF_TRIGGER_RISING.bits | Self::IRQF_TRIGGER_FALLING.bits;
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

impl IrqHandleFlags {
    /// 检查是否指定了触发类型
    #[inline(always)]
    pub fn trigger_type_specified(&self) -> bool {
        (self.bits & Self::IRQF_TRIGGER_MASK.bits) != 0
    }

    /// 插入触发类型
    pub fn insert_trigger_type(&mut self, trigger: IrqLineStatus) {
        self.bits |= trigger.trigger_bits() & IrqHandleFlags::IRQF_TRIGGER_MASK.bits;
    }

    #[allow(dead_code)]
    pub fn remove_trigger_type(&mut self, trigger: IrqLineStatus) {
        self.bits &= !(trigger.trigger_bits() & IrqHandleFlags::IRQF_TRIGGER_MASK.bits);
    }

    pub fn trigger_type(&self) -> IrqLineStatus {
        IrqLineStatus::from_bits_truncate(self.bits & IrqHandleFlags::IRQF_TRIGGER_MASK.bits)
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

    unsafe {
        IRQ_DESC_MANAGER = Some(manager);
    }

    return CurrentIrqArch::arch_early_irq_init();
}

static mut IRQ_DESC_MANAGER: Option<IrqDescManager> = None;

/// 获取中断描述符管理器的引用
#[inline(always)]
pub fn irq_desc_manager() -> &'static IrqDescManager {
    return unsafe { IRQ_DESC_MANAGER.as_ref().unwrap() };
}

pub struct IrqDescManager {
    irq_descs: BTreeMap<IrqNumber, Arc<IrqDesc>>,
}

impl IrqDescManager {
    fn new() -> Self {
        IrqDescManager {
            irq_descs: BTreeMap::new(),
        }
    }

    /// 查找中断描述符
    pub fn lookup(&self, irq: IrqNumber) -> Option<Arc<IrqDesc>> {
        self.irq_descs.get(&irq).map(|desc| desc.clone())
    }

    /// 查找中断描述符并锁定总线(没有对irqdesc进行加锁)
    #[allow(dead_code)]
    pub fn lookup_and_lock_bus(
        &self,
        irq: IrqNumber,
        check_global: bool,
        check_percpu: bool,
    ) -> Option<Arc<IrqDesc>> {
        self.do_lookup_and_lock(irq, true, check_global, check_percpu)
    }

    fn do_lookup_and_lock(
        &self,
        irq: IrqNumber,
        lock_bus: bool,
        check_global: bool,
        check_percpu: bool,
    ) -> Option<Arc<IrqDesc>> {
        let desc = self.lookup(irq)?;
        if check_global || check_percpu {
            if check_percpu && !desc.inner().line_status().is_per_cpu_devid() {
                return None;
            }

            if check_global && desc.inner().line_status().is_per_cpu_devid() {
                return None;
            }
        }

        if lock_bus {
            desc.chip_bus_lock();
        }

        return Some(desc);
    }

    fn insert(&mut self, irq: IrqNumber, desc: Arc<IrqDesc>) {
        self.irq_descs.insert(irq, desc);
    }

    /// 释放中断描述符
    #[allow(dead_code)]
    fn free_desc(&mut self, irq: IrqNumber) {
        if let Some(desc) = self.irq_descs.get(&irq) {
            irq_sysfs_del(desc);
            self.irq_descs.remove(&irq);
        }
    }

    /// 迭代中断描述符
    pub fn iter_descs(&self) -> btree_map::Iter<'_, IrqNumber, Arc<IrqDesc>> {
        self.irq_descs.iter()
    }

    /// 设置指定irq的可用cpu为所有cpu
    pub fn set_percpu_devid_all(&self, irq: IrqNumber) -> Result<(), SystemError> {
        self.set_percpu_devid(irq, None)
    }

    /// 设置指定irq的可用cpu
    ///
    /// 如果affinity为None，则表示设置为所有cpu
    pub fn set_percpu_devid(
        &self,
        irq: IrqNumber,
        affinity: Option<&CpuMask>,
    ) -> Result<(), SystemError> {
        let desc = self.lookup(irq).ok_or(SystemError::EINVAL)?;
        let mut desc_inner = desc.inner();

        if desc_inner.percpu_enabled().is_some() {
            return Err(SystemError::EINVAL);
        }

        *desc_inner.percpu_enabled_mut() = Some(CpuMask::new());

        if let Some(affinity) = affinity {
            desc_inner.percpu_affinity_mut().replace(affinity.clone());
        } else {
            desc_inner
                .percpu_affinity_mut()
                .replace(smp_cpu_manager().possible_cpus().clone());
        }

        drop(desc_inner);

        desc.set_percpu_devid_flags();

        return Ok(());
    }
}
