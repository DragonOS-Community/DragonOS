use super::*;

#[derive(Clone)]
pub struct UprobeTaskScope(Arc<UprobeTaskScopeToken>);

/// The global weak reference keeps the PCB allocation from being reused while
/// a scope exists. The pointer cookie can therefore be compared on the hit
/// path without taking the global scope lock.
struct UprobeTaskScopeToken {
    id: u64,
    target_ptr: usize,
    terminal: AtomicBool,
}

impl Drop for UprobeTaskScopeToken {
    fn drop(&mut self) {
        UPROBE_TASK_SCOPES.lock_irqsave().remove(&self.id);
    }
}

impl UprobeTaskScope {
    pub fn new(target: &Arc<ProcessControlBlock>) -> Self {
        let id = NEXT_TASK_SCOPE_ID.fetch_add(1, Ordering::Relaxed);
        UPROBE_TASK_SCOPES
            .lock_irqsave()
            .insert(id, Arc::downgrade(target));
        Self(Arc::new(UprobeTaskScopeToken {
            id,
            target_ptr: Arc::as_ptr(target) as usize,
            terminal: AtomicBool::new(false),
        }))
    }

    fn permits_task(&self, current: &Arc<ProcessControlBlock>) -> bool {
        !self.0.terminal.load(Ordering::Acquire)
            && Arc::as_ptr(current) as usize == self.0.target_ptr
    }

    fn target(&self) -> Option<Arc<ProcessControlBlock>> {
        let target = { UPROBE_TASK_SCOPES.lock_irqsave().get(&self.0.id).cloned() };
        target.and_then(|target| target.upgrade())
    }

    fn target_mm(&self) -> Option<Arc<AddressSpace>> {
        if self.0.terminal.load(Ordering::Acquire) {
            return None;
        }
        self.target().and_then(|task| task.basic().user_vm())
    }

    fn target_ptr(&self) -> usize {
        self.0.target_ptr
    }

    fn mark_terminal(&self) {
        self.0.terminal.store(true, Ordering::Release);
    }

    fn is_terminal(&self) -> bool {
        self.0.terminal.load(Ordering::Acquire)
    }
}

pub enum UprobeConsumerScope {
    Task(UprobeTaskScope),
    SystemWideAuthorized,
}

impl UprobeConsumerScope {
    pub(super) fn permits(&self, mm: &Arc<AddressSpace>) -> bool {
        match self {
            Self::Task(target) => target
                .target_mm()
                .is_some_and(|target_mm| Arc::ptr_eq(&target_mm, mm)),
            Self::SystemWideAuthorized => true,
        }
    }

    fn task_scope(&self) -> Option<UprobeTaskScope> {
        match self {
            Self::Task(scope) => Some(scope.clone()),
            Self::SystemWideAuthorized => None,
        }
    }

    fn is_terminal(&self) -> bool {
        matches!(self, Self::Task(scope) if scope.is_terminal())
    }
}

pub struct UprobeConsumerRuntime {
    pub event_callback: Option<Arc<dyn uprobe::CallBackFunc>>,
}

#[derive(Clone)]
pub struct UprobeConsumerRuntimeSnapshot {
    endpoint: Arc<UprobeDeliveryEndpoint>,
}

impl UprobeConsumerRuntimeSnapshot {
    pub fn deliver(&self, current: &Arc<ProcessControlBlock>, args: &dyn uprobe::ProbeArgs) {
        if !self
            .endpoint
            .task_scope
            .as_ref()
            .is_none_or(|scope| scope.permits_task(current))
        {
            return;
        }
        let Some(_guard) = self.endpoint.gate.try_enter() else {
            return;
        };
        if let Some(callback) = self.endpoint.callback.as_ref().and_then(Weak::upgrade) {
            callback.call(args);
        }
    }
}

const GATE_STATE_SHIFT: usize = usize::BITS as usize - 2;
const GATE_COUNT_MASK: usize = (1usize << GATE_STATE_SHIFT) - 1;
const GATE_PENDING: usize = 0;
const GATE_OPEN: usize = 1usize << GATE_STATE_SHIFT;
const GATE_CLOSED: usize = 2usize << GATE_STATE_SHIFT;

/// One-shot admission gate. State transitions and reader admission share one
/// atomic modification order, so close either observes a reader or prevents it
/// from entering. A closed gate is never reopened.
struct UprobeAdmissionGate {
    state_and_readers: AtomicUsize,
    wait: WaitQueue,
}

impl UprobeAdmissionGate {
    fn pending() -> Self {
        Self {
            state_and_readers: AtomicUsize::new(GATE_PENDING),
            wait: WaitQueue::default(),
        }
    }

    fn open() -> Self {
        Self {
            state_and_readers: AtomicUsize::new(GATE_OPEN),
            wait: WaitQueue::default(),
        }
    }

    fn publish(&self) -> bool {
        self.state_and_readers
            .compare_exchange(
                GATE_PENDING,
                GATE_OPEN,
                Ordering::Release,
                Ordering::Acquire,
            )
            .is_ok()
    }

    fn try_acquire(&self) -> bool {
        let mut observed = self.state_and_readers.load(Ordering::Acquire);
        loop {
            if observed & !GATE_COUNT_MASK != GATE_OPEN
                || observed & GATE_COUNT_MASK == GATE_COUNT_MASK
            {
                return false;
            }
            match self.state_and_readers.compare_exchange_weak(
                observed,
                observed + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(current) => observed = current,
            }
        }
    }

    fn try_enter(&self) -> Option<UprobeAdmissionGuard<'_>> {
        self.try_acquire().then_some(UprobeAdmissionGuard(self))
    }

    fn leave(&self) {
        let previous = self.state_and_readers.fetch_sub(1, Ordering::Release);
        if previous & GATE_COUNT_MASK == 1 {
            self.wait.wakeup_all(None);
        }
    }

    fn close(&self) {
        let mut observed = self.state_and_readers.load(Ordering::Acquire);
        loop {
            if observed & !GATE_COUNT_MASK == GATE_CLOSED {
                return;
            }
            let closed = GATE_CLOSED | (observed & GATE_COUNT_MASK);
            match self.state_and_readers.compare_exchange_weak(
                observed,
                closed,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(current) => observed = current,
            }
        }
    }

    fn wait_idle(&self) {
        self.wait.wait_until(|| {
            (self.state_and_readers.load(Ordering::Acquire) & GATE_COUNT_MASK == 0).then_some(())
        });
    }
}

struct UprobeAdmissionGuard<'a>(&'a UprobeAdmissionGate);

impl Drop for UprobeAdmissionGuard<'_> {
    fn drop(&mut self) {
        self.0.leave();
    }
}

struct UprobeDeliveryEndpoint {
    gate: UprobeAdmissionGate,
    callback: Option<Weak<dyn uprobe::CallBackFunc>>,
    task_scope: Option<UprobeTaskScope>,
}

struct UprobeConsumerEpoch {
    install_gate: UprobeAdmissionGate,
    delivery: Arc<UprobeDeliveryEndpoint>,
}

impl UprobeConsumerEpoch {
    fn new(runtime: &UprobeConsumerRuntime, task_scope: Option<UprobeTaskScope>) -> Arc<Self> {
        Arc::new(Self {
            install_gate: UprobeAdmissionGate::open(),
            delivery: Arc::new(UprobeDeliveryEndpoint {
                gate: UprobeAdmissionGate::pending(),
                callback: runtime.event_callback.as_ref().map(Arc::downgrade),
                task_scope,
            }),
        })
    }

    fn close_and_drain(&self) {
        self.install_gate.close();
        self.delivery.gate.close();
        self.install_gate.wait_idle();
        self.delivery.gate.wait_idle();
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UprobeConsumerPhase {
    Disabled,
    Enabling,
    Enabled,
    Terminal,
    Closing,
}

struct UprobeConsumerControl {
    phase: UprobeConsumerPhase,
    epoch: Option<Arc<UprobeConsumerEpoch>>,
}

struct InstalledSiteRef {
    mm: Weak<AddressSpace>,
    site: Weak<UprobeSite>,
}

pub struct UprobeConsumer {
    pub(super) id: u64,
    pub(super) definition: Arc<UprobeDefinition>,
    pub(super) scope: UprobeConsumerScope,
    runtime: UprobeConsumerRuntime,
    control: Mutex<UprobeConsumerControl>,
    published_epoch: crate::rcu::RcuOptionArcSlot<UprobeConsumerEpoch>,
    sites: SpinLock<BTreeMap<(u64, usize), InstalledSiteRef>>,
}

pub(super) struct ConsumerInstallGuard {
    epoch: Arc<UprobeConsumerEpoch>,
}

impl ConsumerInstallGuard {
    pub(super) fn hit_target(&self) -> UprobeConsumerRuntimeSnapshot {
        UprobeConsumerRuntimeSnapshot {
            endpoint: self.epoch.delivery.clone(),
        }
    }
}

impl Drop for ConsumerInstallGuard {
    fn drop(&mut self) {
        self.epoch.install_gate.leave();
    }
}

impl UprobeConsumer {
    pub(super) fn has_published_epoch(&self) -> bool {
        self.published_epoch.load().is_some()
    }

    pub fn new(
        id: u64,
        definition: Arc<UprobeDefinition>,
        scope: UprobeConsumerScope,
        runtime: UprobeConsumerRuntime,
        enabled: bool,
    ) -> Arc<Self> {
        let task_scope = scope.task_scope();
        let epoch = enabled.then(|| UprobeConsumerEpoch::new(&runtime, task_scope));
        if let Some(epoch) = epoch.as_ref() {
            let published = epoch.delivery.gate.publish();
            debug_assert!(published);
        }
        let published_epoch = epoch
            .as_ref()
            .map_or_else(crate::rcu::RcuOptionArcSlot::new_none, |epoch| {
                crate::rcu::RcuOptionArcSlot::new_some(epoch.clone())
            });
        Arc::new(Self {
            id,
            definition,
            scope,
            runtime,
            control: Mutex::new(UprobeConsumerControl {
                phase: if enabled {
                    UprobeConsumerPhase::Enabled
                } else {
                    UprobeConsumerPhase::Disabled
                },
                epoch,
            }),
            published_epoch,
            sites: SpinLock::new(BTreeMap::new()),
        })
    }

    pub(super) fn begin_install(
        self: &Arc<Self>,
        mm: &Arc<AddressSpace>,
    ) -> Option<ConsumerInstallGuard> {
        if !self.scope.permits(mm) {
            return None;
        }
        let epoch = self.published_epoch.load()?;
        if !epoch.install_gate.try_acquire() {
            return None;
        }
        if !self.scope.permits(mm) {
            epoch.install_gate.leave();
            return None;
        }
        Some(ConsumerInstallGuard { epoch })
    }

    pub(super) fn remember_site(
        &self,
        mm: &Arc<AddressSpace>,
        vaddr: usize,
        site: &Arc<UprobeSite>,
    ) {
        let mut sites = self.sites.lock_irqsave();
        let key = (mm.id(), vaddr);
        if sites.get(&key).is_some_and(|installed| {
            installed
                .site
                .upgrade()
                .is_some_and(|installed_site| Arc::ptr_eq(&installed_site, site))
        }) {
            return;
        }
        sites.insert(
            key,
            InstalledSiteRef {
                mm: Arc::downgrade(mm),
                site: Arc::downgrade(site),
            },
        );
    }

    pub(super) fn forget_site(&self, mm_id: u64, vaddr: usize, site: &Arc<UprobeSite>) {
        let mut sites = self.sites.lock_irqsave();
        let key = (mm_id, vaddr);
        if sites.get(&key).is_some_and(|installed| {
            installed
                .site
                .upgrade()
                .is_none_or(|installed_site| Arc::ptr_eq(&installed_site, site))
        }) {
            sites.remove(&key);
        }
    }
}

pub struct UprobeConsumerReg {
    pub definition: Arc<UprobeDefinition>,
    pub scope: UprobeConsumerScope,
    pub event_callback: Option<Arc<dyn uprobe::CallBackFunc>>,
    pub enabled: bool,
}

/// 全局注册表：inode id → 文件偏移 → （消费者 id，回调）。
/// 注册表值类型：某（inode, offset）上的消费者列表。
pub(super) type ConsumerList = Vec<Arc<UprobeConsumer>>;
/// 注册表类型：inode id → （文件偏移 → 消费者列表）。
type RegistryMap = BTreeMap<usize, BTreeMap<usize, ConsumerList>>;

pub(super) static UPROBE_REGISTRY: SpinLock<RegistryMap> = SpinLock::new(BTreeMap::new());

static NEXT_CONSUMER_ID: AtomicU64 = AtomicU64::new(1);
static ACTIVE_UPROBE_CONSUMERS: AtomicUsize = AtomicUsize::new(0);
static NEXT_TASK_SCOPE_ID: AtomicU64 = AtomicU64::new(1);
static UPROBE_TASK_SCOPES: SpinLock<BTreeMap<u64, Weak<ProcessControlBlock>>> =
    SpinLock::new(BTreeMap::new());
/// Target PCB pointer -> task-scoped consumers. The target scope's Weak keeps
/// the PCB allocation from being reused until the consumer is removed.
static UPROBE_TASK_CONSUMERS: SpinLock<BTreeMap<usize, Vec<Weak<UprobeConsumer>>>> =
    SpinLock::new(BTreeMap::new());

pub(super) fn uprobe_registry_is_empty() -> bool {
    ACTIVE_UPROBE_CONSUMERS.load(Ordering::Acquire) == 0
}

pub(super) fn uprobe_registry_has_active_range(inode_key: usize, start: usize, end: usize) -> bool {
    if start >= end || uprobe_registry_is_empty() {
        return false;
    }
    let registry = UPROBE_REGISTRY.lock_irqsave();
    registry.get(&inode_key).is_some_and(|offsets| {
        offsets.range(start..end).any(|(_, consumers)| {
            consumers
                .iter()
                .any(|consumer| consumer.has_published_epoch())
        })
    })
}

/// 分配新的消费者 id（每次 perf_event_open(uprobe) 一次）。
pub fn uprobe_new_consumer_id() -> u64 {
    NEXT_CONSUMER_ID.fetch_add(1, Ordering::Relaxed)
}

/// 注册一个消费者探测点（inode + offset）。
pub fn uprobe_registry_add(
    inode_id: usize,
    offset: usize,
    consumer_id: u64,
    reg: Arc<UprobeConsumerReg>,
) -> Arc<UprobeConsumer> {
    debug_assert_eq!(inode_id, reg.definition.inode_id());
    debug_assert_eq!(offset, reg.definition.offset());
    let consumer = UprobeConsumer::new(
        consumer_id,
        reg.definition.clone(),
        match &reg.scope {
            UprobeConsumerScope::Task(task) => UprobeConsumerScope::Task(task.clone()),
            UprobeConsumerScope::SystemWideAuthorized => UprobeConsumerScope::SystemWideAuthorized,
        },
        UprobeConsumerRuntime {
            event_callback: reg.event_callback.clone(),
        },
        reg.enabled,
    );
    uprobe_registry_add_consumer(consumer.clone());
    consumer
}

pub fn uprobe_registry_add_consumer(consumer: Arc<UprobeConsumer>) {
    register_task_consumer(&consumer);
    if matches!(
        consumer.control.lock().phase,
        UprobeConsumerPhase::Enabling | UprobeConsumerPhase::Enabled
    ) && !consumer.scope.is_terminal()
    {
        ACTIVE_UPROBE_CONSUMERS.fetch_add(1, Ordering::AcqRel);
    }
    let mut r = UPROBE_REGISTRY.lock_irqsave();
    r.entry(consumer.definition.inode_key)
        .or_default()
        .entry(consumer.definition.offset())
        .or_default()
        .push(consumer);
}

fn register_task_consumer(consumer: &Arc<UprobeConsumer>) {
    let UprobeConsumerScope::Task(scope) = &consumer.scope else {
        return;
    };
    let mut task_consumers = UPROBE_TASK_CONSUMERS.lock_irqsave();
    let target_is_alive = scope.target().is_some_and(|target| {
        !target.flags().contains(ProcessFlags::EXITING) && target.basic().user_vm().is_some()
    });
    if !target_is_alive {
        scope.mark_terminal();
        let mut control = consumer.control.lock();
        let epoch = control.epoch.take();
        control.phase = UprobeConsumerPhase::Terminal;
        consumer.published_epoch.store_deferred(None);
        drop(control);
        if let Some(epoch) = epoch {
            epoch.close_and_drain();
        }
        return;
    }
    task_consumers
        .entry(scope.target_ptr())
        .or_default()
        .push(Arc::downgrade(consumer));
}

fn unregister_task_consumer(consumer: &UprobeConsumer) {
    let UprobeConsumerScope::Task(scope) = &consumer.scope else {
        return;
    };
    let mut task_consumers = UPROBE_TASK_CONSUMERS.lock_irqsave();
    let remove_key = if let Some(consumers) = task_consumers.get_mut(&scope.target_ptr()) {
        consumers.retain(|indexed| {
            indexed
                .upgrade()
                .is_some_and(|indexed| indexed.id != consumer.id)
        });
        consumers.is_empty()
    } else {
        false
    };
    if remove_key {
        task_consumers.remove(&scope.target_ptr());
    }
}

/// Update the single consumer-level enable state used by both already armed
/// and later-installed sites.
pub fn uprobe_registry_set_enabled(
    consumer: &Arc<UprobeConsumer>,
    enabled: bool,
) -> Result<(), SystemError> {
    let mut control = consumer.control.lock();
    if enabled {
        match control.phase {
            UprobeConsumerPhase::Enabled => return Ok(()),
            UprobeConsumerPhase::Terminal => return Ok(()),
            UprobeConsumerPhase::Closing => return Err(SystemError::ENOENT),
            UprobeConsumerPhase::Enabling => return Err(SystemError::EBUSY),
            UprobeConsumerPhase::Disabled => {}
        }
        let epoch = UprobeConsumerEpoch::new(&consumer.runtime, consumer.scope.task_scope());
        control.phase = UprobeConsumerPhase::Enabling;
        control.epoch = Some(epoch.clone());
        consumer.published_epoch.store_deferred(Some(epoch.clone()));
        ACTIVE_UPROBE_CONSUMERS.fetch_add(1, Ordering::AcqRel);
        if let Err(e) = apply_consumer_to_existing_mappings(consumer) {
            control.epoch.take();
            control.phase = UprobeConsumerPhase::Disabled;
            consumer.published_epoch.store_deferred(None);
            ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
            epoch.close_and_drain();
            detach_consumer_sites(consumer);
            return Err(e);
        }
        control.phase = UprobeConsumerPhase::Enabled;
        let published = epoch.delivery.gate.publish();
        debug_assert!(published);
    } else {
        match control.phase {
            UprobeConsumerPhase::Disabled | UprobeConsumerPhase::Terminal => return Ok(()),
            UprobeConsumerPhase::Closing => return Err(SystemError::ENOENT),
            UprobeConsumerPhase::Enabling => return Err(SystemError::EBUSY),
            UprobeConsumerPhase::Enabled => {}
        }
        let epoch = control.epoch.take().expect("enabled uprobe without epoch");
        control.phase = UprobeConsumerPhase::Disabled;
        consumer.published_epoch.store_deferred(None);
        ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
        epoch.close_and_drain();
        detach_consumer_sites(consumer);
    }
    Ok(())
}

fn detach_consumer_sites(consumer: &UprobeConsumer) {
    let installed = core::mem::take(&mut *consumer.sites.lock_irqsave());
    for ((_, vaddr), installed) in installed {
        if let (Some(mm), Some(site)) = (installed.mm.upgrade(), installed.site.upgrade()) {
            uprobe_unregister_consumer_from_site(&mm, vaddr, &site, consumer.id);
        }
    }
}

fn detach_consumer_from_mm(consumer: &UprobeConsumer, mm: &Arc<AddressSpace>) {
    let mm_id = mm.id();
    let installed = {
        let mut sites = consumer.sites.lock_irqsave();
        let keys: Vec<(u64, usize)> = sites
            .range((mm_id, 0)..=(mm_id, usize::MAX))
            .map(|(key, _)| *key)
            .collect();
        keys.into_iter()
            .filter_map(|key| sites.remove(&key).map(|site| (key.1, site)))
            .collect::<Vec<_>>()
    };
    for (vaddr, installed) in installed {
        if let Some(site) = installed.site.upgrade() {
            uprobe_unregister_consumer_from_site(mm, vaddr, &site, consumer.id);
        }
    }
}

/// Put task-scoped events into their terminal state before the target drops
/// its mm. The perf fd and its final count stay alive, but no site can be
/// installed or re-enabled afterwards.
pub fn uprobe_registry_task_exit(target: &Arc<ProcessControlBlock>) {
    let target_ptr = Arc::as_ptr(target) as usize;
    let consumers = {
        let mut task_consumers = UPROBE_TASK_CONSUMERS.lock_irqsave();
        task_consumers
            .remove(&target_ptr)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|consumer| {
                let consumer = consumer.upgrade()?;
                if let UprobeConsumerScope::Task(scope) = &consumer.scope {
                    scope.mark_terminal();
                }
                Some(consumer)
            })
            .collect::<Vec<_>>()
    };

    for consumer in consumers {
        let UprobeConsumerScope::Task(_) = &consumer.scope else {
            continue;
        };
        let mut control = consumer.control.lock();
        let was_active = matches!(
            control.phase,
            UprobeConsumerPhase::Enabling | UprobeConsumerPhase::Enabled
        );
        let epoch = control.epoch.take();
        control.phase = UprobeConsumerPhase::Terminal;
        consumer.published_epoch.store_deferred(None);
        if was_active {
            ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
        }
        if let Some(epoch) = epoch {
            epoch.close_and_drain();
        }
        detach_consumer_sites(&consumer);
    }
}

/// A successful exec keeps a task event alive on the new mm. Remove only the
/// old-mm memberships, which may otherwise remain armed when another
/// CLONE_VM task still owns that address space.
pub fn uprobe_registry_task_exec(
    target: &Arc<ProcessControlBlock>,
    old_mm: Option<&Arc<AddressSpace>>,
) {
    let Some(old_mm) = old_mm else { return };
    let target_ptr = Arc::as_ptr(target) as usize;
    let consumers = {
        let task_consumers = UPROBE_TASK_CONSUMERS.lock_irqsave();
        task_consumers
            .get(&target_ptr)
            .into_iter()
            .flatten()
            .filter_map(|consumer| consumer.upgrade())
            .collect::<Vec<_>>()
    };
    for consumer in consumers {
        let control = consumer.control.lock();
        if matches!(
            control.phase,
            UprobeConsumerPhase::Closing | UprobeConsumerPhase::Terminal
        ) {
            continue;
        }
        detach_consumer_from_mm(&consumer, old_mm);
    }
}

fn apply_consumer_to_existing_mappings(consumer: &Arc<UprobeConsumer>) -> Result<(), SystemError> {
    if let UprobeConsumerScope::Task(target) = &consumer.scope {
        let Some(mm) = target.target_mm() else {
            return Ok(());
        };
        return apply_consumer_to_mm(consumer, &mm);
    }

    let page_cache = consumer
        .definition
        .inode()
        .page_cache()
        .ok_or(SystemError::EINVAL)?;
    for vma in page_cache.collect_file_vmas() {
        let mapping = {
            let guard = vma.lock();
            let Some(mm) = guard.address_space().and_then(|owner| owner.upgrade()) else {
                continue;
            };
            let Some(pgoff) = guard.backing_page_offset() else {
                continue;
            };
            let Some(file) = guard.vm_file() else {
                continue;
            };
            (mm, file, *guard.region(), pgoff)
        };
        uprobe_apply_to_new_vma_inner(
            &mapping.0,
            &mapping.1,
            mapping.2.start().data(),
            mapping.2.size(),
            mapping.3 << MMArch::PAGE_SHIFT,
            true,
            Some(consumer.id),
        )?;
    }
    Ok(())
}

fn apply_consumer_to_mm(
    consumer: &Arc<UprobeConsumer>,
    mm: &Arc<AddressSpace>,
) -> Result<(), SystemError> {
    let all = VirtRegion::new(VirtAddr::new(0), MMArch::USER_END_VADDR.data());
    let definition_offset = consumer.definition.offset();
    for (file, start, size, file_start) in collect_file_vma_snapshot(mm, all) {
        if !consumer.definition.matches_inode(&file.inode()) {
            continue;
        }
        let Some(file_end) = file_start.checked_add(size) else {
            return Err(SystemError::EINVAL);
        };
        if definition_offset < file_start || definition_offset >= file_end {
            continue;
        }
        uprobe_apply_to_new_vma_inner(mm, &file, start, size, file_start, true, Some(consumer.id))?;
    }
    Ok(())
}
/// 消费者关闭：移除注册表项 + drop 迟到句柄（逐 mm 注销）。
pub fn uprobe_registry_remove_consumer(consumer: &Arc<UprobeConsumer>) {
    let mut control = consumer.control.lock();
    if control.phase == UprobeConsumerPhase::Closing {
        return;
    }
    let was_active = matches!(
        control.phase,
        UprobeConsumerPhase::Enabling | UprobeConsumerPhase::Enabled
    );
    let epoch = control.epoch.take();
    control.phase = UprobeConsumerPhase::Closing;
    consumer.published_epoch.store_deferred(None);
    if was_active {
        ACTIVE_UPROBE_CONSUMERS.fetch_sub(1, Ordering::AcqRel);
    }
    drop(control);

    let removed = {
        let mut r = UPROBE_REGISTRY.lock_irqsave();
        let inode_key = consumer.definition.inode_key;
        let offset = consumer.definition.offset();
        let removed = r
            .get_mut(&inode_key)
            .and_then(|offsets| offsets.get_mut(&offset))
            .and_then(|consumers| {
                consumers
                    .iter()
                    .position(|candidate| Arc::ptr_eq(candidate, consumer))
                    .map(|index| consumers.remove(index))
            });
        if let Some(offsets) = r.get_mut(&inode_key) {
            offsets.retain(|_, consumers| !consumers.is_empty());
            if offsets.is_empty() {
                r.remove(&inode_key);
            }
        }
        removed
    };
    let Some(removed) = removed else { return };
    debug_assert!(Arc::ptr_eq(consumer, &removed));
    unregister_task_consumer(consumer);
    if let Some(epoch) = epoch {
        epoch.close_and_drain();
    }
    let sites = core::mem::take(&mut *consumer.sites.lock_irqsave());
    for ((_, vaddr), installed) in sites {
        if let (Some(mm), Some(site)) = (installed.mm.upgrade(), installed.site.upgrade()) {
            uprobe_unregister_consumer_from_site(&mm, vaddr, &site, consumer.id);
        }
    }
}
