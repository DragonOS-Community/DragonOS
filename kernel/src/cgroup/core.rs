use alloc::{
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::cmp::Reverse;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use hashbrown::{HashMap, HashSet};
use system_error::SystemError;

use crate::{
    bpf::prog::{device::DeviceAccess, BpfProg},
    cgroup::{CgroupCpuState, CgroupMemoryState},
    include::bindings::linux_bpf::{
        bpf_prog_type, BPF_F_ALLOW_MULTI, BPF_F_ALLOW_OVERRIDE, BPF_F_REPLACE,
    },
    libs::{mutex::Mutex, rwlock::RwLock, spinlock::SpinLock},
    process::{ProcessManager, RawPid},
};

/// `BPF_F_PREORDER` is not generated in the current Linux BPF bindings.
pub const BPF_DEVICE_F_PREORDER: u32 = 1 << 6;
const BPF_CGROUP_MAX_PROGS: usize = 64;
type DeviceSnapshotUpdates = Vec<(Arc<CgroupNode>, Arc<Vec<Arc<BpfProg>>>)>;

#[derive(Debug, Clone)]
struct AttachedDeviceProgram {
    prog: Arc<BpfProg>,
    flags: u32,
}

#[derive(Debug)]
struct DeviceBpfState {
    direct: Vec<AttachedDeviceProgram>,
    flags: u32,
    /// Immutable effective chain; readers only clone this Arc under the node lock.
    effective: Arc<Vec<Arc<BpfProg>>>,
}

impl DeviceBpfState {
    fn empty() -> Self {
        Self {
            direct: Vec::new(),
            flags: 0,
            effective: Arc::new(Vec::new()),
        }
    }
}

/// Canonical DAC identity, shared by all mount views of a cgroup file.
#[derive(Debug, Clone, Copy)]
pub(crate) struct CgroupFilePermissions {
    pub uid: u32,
    pub gid: u32,
    pub mode: u32,
    pub generation: u64,
}

impl CgroupFilePermissions {
    /// Match Linux 6.6 kernfs_new_node followed by cgroup_kn_set_ugid.
    /// Nonzero creator credentials override inherited ownership; (0, 0)
    /// leaves kernfs's initial ownership, including SGID group inheritance.
    pub(crate) fn for_creation(
        parent: Self,
        uid: u32,
        gid: u32,
        mode: u32,
        directory: bool,
    ) -> Self {
        let sgid = parent.mode & 0o2000 != 0;
        Self {
            uid,
            gid: if uid == 0 && gid == 0 && sgid {
                parent.gid
            } else {
                gid
            },
            mode: mode | if directory && sgid { 0o2000 } else { 0 },
            generation: 0,
        }
    }
}

#[derive(Debug)]
pub struct CgroupNode {
    id: usize,
    name: String,
    parent: Option<Weak<CgroupNode>>,
    children: RwLock<HashMap<String, Arc<CgroupNode>>>,
    pub(super) threaded: AtomicBool,
    file_permissions: RwLock<HashMap<String, Arc<RwLock<CgroupFilePermissions>>>>,
    tasks: RwLock<HashSet<RawPid>>,
    subtree_control: RwLock<HashSet<String>>,
    cpu: RwLock<CgroupCpuState>,
    cpu_accounting: Arc<super::cpu::accounting::CpuAccounting>,
    /// Optional online CPU css. Scheduler paths use the PCB's stable cache,
    /// never this sleeping configuration lock. Root uses the existing root rq.
    cpu_group: RwLock<Option<Arc<crate::sched::TaskGroup>>>,
    cpuset: RwLock<super::cpuset::CpusetState>,
    memory: RwLock<CgroupMemoryState>,
    /// Physical resource ownership outlives directory and task membership.
    memory_current: RwLock<Option<MemoryGeneration>>,
    pub(super) freezer: super::freezer::NodeState,
    pids_max: RwLock<Option<usize>>,
    pids_events_max: AtomicU64,
    pids_account: Arc<super::pids_account::PidsAccount>,
    /// Inclusive live population, serialized by cgroup_accounting_lock.
    populated_tasks: AtomicUsize,
    core_events_sequence: AtomicU64,
    device_bpf: RwLock<DeviceBpfState>,
}

#[derive(Clone, Debug)]
pub(crate) struct MemoryGeneration {
    pub account: Arc<super::memory::MemoryAccount>,
    pub object_account: Arc<super::memory::object::ObjectAccount>,
}

impl MemoryGeneration {
    pub(crate) fn prepare(
        id: usize,
        parent: Option<Arc<super::memory::MemoryAccount>>,
    ) -> Result<(Self, super::memory::object::PreparedObjectAccount), SystemError> {
        let account = super::memory::MemoryAccount::new(id as u64, parent)?;
        let prepared = super::memory::object::ObjectAccount::prepare(&account)?;
        let generation = Self {
            account,
            object_account: prepared.object_account().clone(),
        };
        Ok((generation, prepared))
    }

    pub(crate) fn retire(&self) {
        super::memory::object::offline_memory_account(&self.account)
            .expect("only non-root memory generations retire");
    }
}

pub(crate) struct PreparedFilePermission {
    name: &'static str,
    attributes: Arc<RwLock<CgroupFilePermissions>>,
}

impl CgroupNode {
    fn new_root() -> Arc<Self> {
        let (memory, prepared) =
            MemoryGeneration::prepare(1, None).expect("initial memory account allocation failed");
        prepared.publish();
        Arc::new(Self {
            id: 1,
            name: String::new(),
            parent: None,
            children: RwLock::new(HashMap::new()),
            threaded: AtomicBool::new(false),
            file_permissions: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            cpu: RwLock::new(CgroupCpuState::default()),
            cpu_accounting: super::cpu::accounting::CpuAccounting::new(None),
            cpu_group: RwLock::new(None),
            cpuset: RwLock::new(super::cpuset::CpusetState::default()),
            memory: RwLock::new(CgroupMemoryState::default()),
            memory_current: RwLock::new(Some(memory)),
            freezer: super::freezer::NodeState::default(),
            pids_max: RwLock::new(None),
            pids_events_max: AtomicU64::new(0),
            pids_account: super::pids_account::PidsAccount::try_new(None)
                .expect("initial pids account allocation failed"),
            populated_tasks: AtomicUsize::new(0),
            core_events_sequence: AtomicU64::new(0),
            device_bpf: RwLock::new(DeviceBpfState::empty()),
        })
    }

    fn new_child(
        id: usize,
        name: String,
        parent: &Arc<CgroupNode>,
    ) -> Result<
        (
            Arc<Self>,
            Option<super::memory::object::PreparedObjectAccount>,
        ),
        SystemError,
    > {
        let (memory, prepared) = if parent.subtree_controller_enabled("memory") {
            let (generation, prepared) =
                MemoryGeneration::prepare(id, Some(parent.memory_account()))?;
            (Some(generation), Some(prepared))
        } else {
            (None, None)
        };
        let node = Arc::try_new(Self {
            id,
            name,
            parent: Some(Arc::downgrade(parent)),
            children: RwLock::new(HashMap::new()),
            threaded: AtomicBool::new(false),
            file_permissions: RwLock::new(HashMap::new()),
            tasks: RwLock::new(HashSet::new()),
            subtree_control: RwLock::new(HashSet::new()),
            cpu: RwLock::new(CgroupCpuState::default()),
            cpu_accounting: super::cpu::accounting::CpuAccounting::new(Some(
                parent.cpu_accounting.clone(),
            )),
            cpu_group: RwLock::new(None),
            cpuset: RwLock::new(super::cpuset::CpusetState::default()),
            memory: RwLock::new(CgroupMemoryState::default()),
            memory_current: RwLock::new(memory),
            freezer: super::freezer::NodeState::default(),
            pids_max: RwLock::new(None),
            pids_events_max: AtomicU64::new(0),
            pids_account: super::pids_account::PidsAccount::try_new(Some(
                parent.pids_account.clone(),
            ))?,
            populated_tasks: AtomicUsize::new(0),
            core_events_sequence: AtomicU64::new(0),
            device_bpf: RwLock::new(DeviceBpfState::empty()),
        })
        .map_err(|_| SystemError::ENOMEM)?;
        Ok((node, prepared))
    }

    pub fn id(&self) -> usize {
        self.id
    }

    pub(crate) fn cpu_accounting(&self) -> Arc<super::cpu::accounting::CpuAccounting> {
        self.cpu_accounting.clone()
    }

    pub(crate) fn file_permissions(&self, name: &str, default_mode: u32) -> CgroupFilePermissions {
        *self.file_permissions_instance(name, default_mode).read()
    }

    /// An inode pins this instance, not the name. Recreated controller files
    /// receive a new instance while existing references retain their old DAC.
    pub(crate) fn file_permissions_instance(
        &self,
        name: &str,
        default_mode: u32,
    ) -> Arc<RwLock<CgroupFilePermissions>> {
        if let Some(attrs) = self.file_permissions.read().get(name).cloned() {
            return attrs;
        }
        self.file_permissions
            .write()
            .entry(name.to_string())
            .or_insert_with(|| {
                Arc::new(RwLock::new(CgroupFilePermissions {
                    uid: 0,
                    gid: 0,
                    mode: default_mode,
                    generation: 0,
                }))
            })
            .clone()
    }

    pub(crate) fn set_file_permissions(&self, name: &str, permissions: CgroupFilePermissions) {
        let instance = self.file_permissions_instance(name, permissions.mode);
        let mut attrs = instance.write();
        *attrs = CgroupFilePermissions {
            generation: attrs.generation,
            ..permissions
        };
    }

    pub(crate) fn file_generation(&self, name: &str) -> u64 {
        self.file_permissions
            .read()
            .get(name)
            .map_or(0, |entry| entry.read().generation)
    }

    pub(crate) fn reset_file_permissions(&self, name: &str, default_mode: u32) {
        let cred = ProcessManager::current_pcb().cred();
        let parent = self.file_permissions("", 0o755);
        let mut attrs = self.file_permissions.write();
        let generation = attrs
            .get(name)
            .map_or(0, |entry| entry.read().generation)
            .checked_add(1)
            .expect("cgroup file generation exhausted");
        attrs.insert(
            name.to_string(),
            Arc::new(RwLock::new(CgroupFilePermissions {
                generation,
                ..CgroupFilePermissions::for_creation(
                    parent,
                    cred.fsuid.data() as u32,
                    cred.fsgid.data() as u32,
                    default_mode,
                    false,
                )
            })),
        );
    }

    pub(crate) fn prepare_file_permission_reset(
        &self,
        name: &'static str,
        default_mode: u32,
    ) -> Result<PreparedFilePermission, SystemError> {
        let cred = ProcessManager::current_pcb().cred();
        let parent = self.file_permissions.read().get("").map_or(
            CgroupFilePermissions {
                uid: 0,
                gid: 0,
                mode: 0o755,
                generation: 0,
            },
            |entry| *entry.read(),
        );
        let original = super::file_permissions::canonical_instance(
            &self.file_permissions,
            name,
            default_mode,
        )?;
        let generation = original
            .read()
            .generation
            .checked_add(1)
            .ok_or(SystemError::EOVERFLOW)?;
        // UPDATE is the sole epoch-reset owner; chmod preserves the epoch.
        // The replacement allocation must not retain map or attributes locks.
        let attributes = Arc::try_new(RwLock::new(CgroupFilePermissions {
            generation,
            ..CgroupFilePermissions::for_creation(
                parent,
                cred.fsuid.data() as u32,
                cred.fsgid.data() as u32,
                default_mode,
                false,
            )
        }))
        .map_err(|_| SystemError::ENOMEM)?;
        Ok(PreparedFilePermission { name, attributes })
    }

    pub(crate) fn commit_file_permission_reset(
        &self,
        mut prepared: PreparedFilePermission,
    ) -> PreparedFilePermission {
        let mut map = self.file_permissions.write();
        let old = core::mem::replace(
            map.get_mut(prepared.name)
                .expect("prepared permission has a canonical entry"),
            prepared.attributes,
        );
        prepared.attributes = old;
        prepared
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn parent(&self) -> Option<Arc<CgroupNode>> {
        self.parent.as_ref().and_then(|p| p.upgrade())
    }

    /// UPDATE excludes all population growth; exit only removes entries.
    /// Prepare backing outside tasks/accounting/PID locks before publication.
    pub(crate) fn reserve_task_slots(&self, additional: usize) -> Result<(), SystemError> {
        let (len, capacity) = {
            let tasks = self.tasks.read();
            (tasks.len(), tasks.capacity())
        };
        let needed = len.checked_add(additional).ok_or(SystemError::ENOMEM)?;
        if capacity >= needed {
            return Ok(());
        }
        let mut prepared = HashSet::new();
        if prepared.try_reserve(needed).is_err() {
            let tasks = self.tasks.read();
            return if tasks.capacity()
                >= tasks
                    .len()
                    .checked_add(additional)
                    .ok_or(SystemError::ENOMEM)?
            {
                Ok(())
            } else {
                Err(SystemError::ENOMEM)
            };
        }
        {
            let mut tasks = self.tasks.write();
            if tasks.capacity()
                < tasks
                    .len()
                    .checked_add(additional)
                    .ok_or(SystemError::ENOMEM)?
            {
                prepared.extend(tasks.drain());
                core::mem::swap(&mut *tasks, &mut prepared);
            }
        }
        drop(prepared);
        Ok(())
    }

    /// Caller holds accounting and the freezer publication transaction.
    pub(super) fn add_task(&self, pid: RawPid) {
        let mut tasks = self.tasks.write();
        debug_assert!(tasks.len() < tasks.capacity());
        if !tasks.insert(pid) {
            debug_assert!(false, "cgroup task {:?} already exists", pid);
            return;
        }
        drop(tasks);
        self.adjust_population(true);
        let mut cur = self.parent();
        while let Some(node) = cur {
            node.adjust_population(true);
            cur = node.parent();
        }
    }

    /// Caller holds accounting and the freezer publication transaction.
    pub(super) fn remove_task(&self, pid: RawPid) {
        if !self.tasks.write().remove(&pid) {
            debug_assert!(false, "cgroup task {:?} does not exist", pid);
            return;
        }
        self.adjust_population(false);
        let mut cur = self.parent();
        while let Some(node) = cur {
            node.adjust_population(false);
            cur = node.parent();
        }
    }

    fn adjust_population(&self, add: bool) {
        let edge = if add {
            self.populated_tasks.fetch_add(1, Ordering::AcqRel) == 0
        } else {
            let old = self.populated_tasks.fetch_sub(1, Ordering::AcqRel);
            debug_assert_ne!(old, 0, "population underflow");
            old == 1
        };
        if edge {
            self.notify_core_events();
        }
    }

    pub(crate) fn core_events_sequence(&self) -> u64 {
        self.core_events_sequence.load(Ordering::Acquire)
    }

    pub(super) fn notify_core_events(&self) {
        self.core_events_sequence.fetch_add(1, Ordering::Release);
        crate::mm::memory_notify::request_file_events();
    }

    pub(crate) fn is_populated(&self) -> bool {
        self.populated_tasks.load(Ordering::Acquire) != 0
    }

    pub fn rename_task(&self, old_pid: RawPid, new_pid: RawPid) {
        if old_pid == new_pid {
            return;
        }

        let mut tasks = self.tasks.write();
        if !tasks.remove(&old_pid) {
            debug_assert!(false, "cgroup task {:?} does not exist", old_pid);
            return;
        }
        let inserted = tasks.insert(new_pid);
        debug_assert!(inserted, "cgroup task {:?} already exists", new_pid);
    }

    pub fn tasks(&self) -> Vec<RawPid> {
        self.tasks.read().iter().cloned().collect()
    }

    pub(crate) fn tasks_snapshot(&self) -> Result<Vec<RawPid>, SystemError> {
        let tasks = self.tasks.read();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(tasks.len())
            .map_err(|_| SystemError::ENOMEM)?;
        snapshot.extend(tasks.iter().copied());
        Ok(snapshot)
    }

    pub fn children_names(&self) -> Vec<String> {
        self.children.read().keys().cloned().collect()
    }

    pub fn children(&self) -> Vec<Arc<CgroupNode>> {
        self.children.read().values().cloned().collect()
    }

    pub(crate) fn children_snapshot(&self) -> Result<Vec<Arc<CgroupNode>>, SystemError> {
        let children = self.children.read();
        let mut snapshot = Vec::new();
        snapshot
            .try_reserve_exact(children.len())
            .map_err(|_| SystemError::ENOMEM)?;
        snapshot.extend(children.values().cloned());
        Ok(snapshot)
    }

    pub(crate) fn any_child(&self, predicate: impl FnMut(&Arc<CgroupNode>) -> bool) -> bool {
        self.children.read().values().any(predicate)
    }

    pub(crate) fn any_subtree_controller(&self, predicate: impl FnMut(&str) -> bool) -> bool {
        self.subtree_control
            .read()
            .iter()
            .map(String::as_str)
            .any(predicate)
    }

    pub(crate) fn subtree_control_snapshot(&self) -> Result<HashSet<String>, SystemError> {
        let controls = self.subtree_control.read();
        let mut snapshot = HashSet::new();
        snapshot
            .try_reserve(controls.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for name in controls.iter() {
            let mut copy = String::new();
            copy.try_reserve_exact(name.len())
                .map_err(|_| SystemError::ENOMEM)?;
            copy.push_str(name);
            snapshot.insert(copy);
        }
        Ok(snapshot)
    }

    pub fn child(&self, name: &str) -> Option<Arc<CgroupNode>> {
        self.children.read().get(name).cloned()
    }

    pub fn has_children(&self) -> bool {
        !self.children.read().is_empty()
    }

    pub fn has_tasks(&self) -> bool {
        !self.tasks.read().is_empty()
    }

    pub fn subtree_control(&self) -> Vec<String> {
        self.subtree_control.read().iter().cloned().collect()
    }

    /// Query a controller without allocating a copy of its configured names.
    /// Resource-owner lookup must not allocate while deciding where to charge.
    pub(crate) fn subtree_controller_enabled(&self, name: &str) -> bool {
        self.subtree_control.read().contains(name)
    }

    pub fn set_subtree_control(&self, controllers: HashSet<String>) {
        *self.subtree_control.write() = controllers;
    }

    pub(crate) fn replace_subtree_control(&self, controllers: HashSet<String>) -> HashSet<String> {
        core::mem::replace(&mut *self.subtree_control.write(), controllers)
    }

    pub fn cpu_state(&self) -> CgroupCpuState {
        *self.cpu.read()
    }

    /// Publish a validated CPU configuration under cgroup::UPDATE_LOCK.
    pub(crate) fn set_cpu_state(&self, state: CgroupCpuState) {
        *self.cpu.write() = state;
    }

    pub(crate) fn cpu_group(&self) -> Option<Arc<crate::sched::TaskGroup>> {
        self.cpu_group.read().clone()
    }

    /// Topology/configuration callers hold cgroup::UPDATE_LOCK. Retire the old
    /// css only after its tasks have migrated; publication alone must not tear
    /// down queues still pinned by scheduler/task references.
    pub(crate) fn set_cpu_group(&self, group: Option<Arc<crate::sched::TaskGroup>>) {
        let old = {
            let mut current = self.cpu_group.write();
            core::mem::replace(&mut *current, group)
        };
        // A last reference can run cleanup: do not drop it under a node lock.
        drop(old);
    }

    pub(crate) fn reset_controller(&self, name: &str) {
        match name {
            "cpu" => *self.cpu.write() = CgroupCpuState::default(),
            "memory" => *self.memory.write() = CgroupMemoryState::default(),
            "pids" => {
                *self.pids_max.write() = None;
                self.pids_events_max.store(0, Ordering::Release);
            }
            // Cpuset resets both configured masks and placement outside the
            // accounting spinlock, in controller_changed_locked().
            _ => {}
        }
    }

    pub(crate) fn cpuset_state(&self) -> super::cpuset::CpusetState {
        self.cpuset.read().clone()
    }

    pub(crate) fn set_cpuset_state(&self, state: super::cpuset::CpusetState) {
        *self.cpuset.write() = state;
    }

    pub fn memory_state(&self) -> CgroupMemoryState {
        *self.memory.read()
    }

    pub(crate) fn memory_account(&self) -> Arc<super::memory::MemoryAccount> {
        self.effective_memory_generation().account
    }

    pub(crate) fn effective_memory_generation(&self) -> MemoryGeneration {
        if let Some(generation) = self.local_memory_generation() {
            if generation.account.is_online() {
                return generation;
            }
        }
        let mut cursor = self.parent();
        while let Some(node) = cursor {
            if let Some(generation) = node.local_memory_generation() {
                if generation.account.is_online() {
                    return generation;
                }
            }
            cursor = node.parent();
        }
        super::cgroup_root_node()
            .local_memory_generation()
            .expect("root memory generation is permanent")
    }

    pub(crate) fn local_memory_generation(&self) -> Option<MemoryGeneration> {
        self.memory_current.read().clone()
    }

    pub(crate) fn replace_memory_generation(
        &self,
        generation: Option<MemoryGeneration>,
    ) -> Option<MemoryGeneration> {
        core::mem::replace(&mut *self.memory_current.write(), generation)
    }

    pub fn set_memory_swap_high(&self, value: Option<u64>) {
        self.memory.write().set_swap_high(value);
    }

    pub fn set_memory_swap_max(&self, value: Option<u64>) {
        self.memory.write().set_swap_max(value);
    }

    pub fn freeze_requested(&self) -> bool {
        self.freezer.requested()
    }

    pub fn frozen(&self) -> bool {
        self.freezer.frozen()
    }

    pub fn pids_max(&self) -> Option<usize> {
        *self.pids_max.read()
    }

    pub fn set_pids_max(&self, max: Option<usize>) {
        *self.pids_max.write() = max;
    }

    pub fn pids_events_max(&self) -> u64 {
        self.pids_events_max.load(Ordering::Relaxed)
    }

    pub fn inc_pids_events_max(&self) {
        self.pids_events_max.fetch_add(1, Ordering::Relaxed);
    }

    pub fn subtree_task_count(&self) -> usize {
        self.populated_tasks.load(Ordering::Acquire)
    }

    pub fn charge_pids(&self, count: usize) {
        if count == 0 {
            return;
        }

        self.pids_account.charge(count);
    }

    pub fn uncharge_pids(&self, count: usize) {
        if count == 0 {
            return;
        }

        self.pids_account.uncharge(count);
    }

    pub fn transfer_pids_charge(src: &Arc<Self>, dst: &Arc<Self>, count: usize) {
        if count == 0 || Arc::ptr_eq(src, dst) {
            return;
        }

        src.uncharge_pids(count);
        dst.charge_pids(count);
    }

    pub fn pids_current_count(&self) -> usize {
        self.pids_account.current()
    }

    pub fn is_ancestor_of(self: &Arc<Self>, other: &Arc<Self>) -> bool {
        if Arc::ptr_eq(self, other) {
            return true;
        }

        let mut cur = other.parent();
        while let Some(node) = cur {
            if Arc::ptr_eq(self, &node) {
                return true;
            }
            cur = node.parent();
        }

        false
    }

    /// Apply the complete effective chain to one device operation. Linux does
    /// not short-circuit this chain when a program denies access.
    pub fn allows_device_access(&self, access: DeviceAccess) -> bool {
        let programs = self.device_bpf.read().effective.clone();
        let mut allowed = true;
        for program in programs.iter() {
            if !program.run_device(access) {
                allowed = false;
            }
        }
        allowed
    }
}

#[derive(Debug)]
pub struct CgroupRoot {
    root: Arc<CgroupNode>,
    /// Delegation is a hierarchy-wide policy, not a property of a mount view.
    nsdelegate: AtomicBool,
    next_id: AtomicUsize,
    all_nodes: SpinLock<HashMap<usize, Arc<CgroupNode>>>,
    /// Serializes hierarchy changes and device-program state transitions. An
    /// accounting-lock holder must never acquire this sleeping lock.
    structure_lock: Mutex<()>,
}

impl CgroupRoot {
    fn new() -> Arc<Self> {
        let root = CgroupNode::new_root();
        let mut all_nodes = HashMap::new();
        all_nodes.insert(root.id(), root.clone());

        Arc::new(Self {
            root,
            nsdelegate: AtomicBool::new(false),
            next_id: AtomicUsize::new(2),
            all_nodes: SpinLock::new(all_nodes),
            structure_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> Arc<CgroupNode> {
        self.root.clone()
    }

    pub(crate) fn nsdelegate(&self) -> bool {
        self.nsdelegate.load(Ordering::Acquire)
    }

    /// The mount control plane checks the initial cgroup namespace and holds
    /// cgroup::lock() before changing this shared policy.
    pub(crate) fn set_nsdelegate(&self, enabled: bool) {
        self.nsdelegate.store(enabled, Ordering::Release);
    }

    #[allow(dead_code)]
    pub fn lookup_by_id(&self, id: usize) -> Option<Arc<CgroupNode>> {
        self.all_nodes.lock().get(&id).cloned()
    }

    pub fn is_online(&self, node: &Arc<CgroupNode>) -> bool {
        self.all_nodes
            .lock()
            .get(&node.id())
            .is_some_and(|online| Arc::ptr_eq(online, node))
    }

    pub fn create_child(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
    ) -> Result<Arc<CgroupNode>, SystemError> {
        self.create_child_inner(parent, name, false, |_| Ok(()))
    }

    pub(crate) fn create_child_exclusive_with_init(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
        init: impl FnOnce(&Arc<CgroupNode>) -> Result<(), SystemError>,
    ) -> Result<Arc<CgroupNode>, SystemError> {
        self.create_child_inner(parent, name, true, init)
    }

    fn create_child_inner(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
        exclusive: bool,
        init: impl FnOnce(&Arc<CgroupNode>) -> Result<(), SystemError>,
    ) -> Result<Arc<CgroupNode>, SystemError> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(SystemError::EINVAL);
        }
        let _cpuset_guard = super::cpuset::lock();
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(parent) {
            return Err(SystemError::ENOENT);
        }
        if let Some(existing) = parent.children.read().get(name) {
            if exclusive {
                return Err(SystemError::EEXIST);
            }
            return Ok(existing.clone());
        }

        // Both authority maps and their owned key are prepared before the
        // fallible inode initializer. UPDATE/structure exclude other inserts.
        let mut key = String::new();
        key.try_reserve_exact(name.len())
            .map_err(|_| SystemError::ENOMEM)?;
        key.push_str(name);
        let mut child_name = String::new();
        child_name
            .try_reserve_exact(name.len())
            .map_err(|_| SystemError::ENOMEM)?;
        child_name.push_str(name);
        let parent_needed = parent
            .children
            .read()
            .len()
            .checked_add(1)
            .ok_or(SystemError::ENOMEM)?;
        if parent.children.read().capacity() < parent_needed {
            let mut prepared = HashMap::new();
            prepared
                .try_reserve(parent_needed)
                .map_err(|_| SystemError::ENOMEM)?;
            {
                let mut children = parent.children.write();
                prepared.extend(children.drain());
                core::mem::swap(&mut *children, &mut prepared);
            }
            drop(prepared);
        }
        let all_needed = self
            .all_nodes
            .lock()
            .len()
            .checked_add(1)
            .ok_or(SystemError::ENOMEM)?;
        if self.all_nodes.lock().capacity() < all_needed {
            let mut prepared = HashMap::new();
            prepared
                .try_reserve(all_needed)
                .map_err(|_| SystemError::ENOMEM)?;
            {
                let mut all = self.all_nodes.lock();
                prepared.extend(all.drain());
                core::mem::swap(&mut *all, &mut prepared);
            }
            drop(prepared);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (child, memory_prepared) = CgroupNode::new_child(id, child_name, parent)?;
        super::freezer::initialize(&child, parent);
        child.device_bpf.write().effective = parent.device_bpf.read().effective.clone();
        super::cpu::initialize_node_locked(&child)?;
        init(&child)?;
        parent.children.write().insert(key, child.clone());

        self.all_nodes.lock().insert(id, child.clone());
        drop(_structure_guard);
        if let Some(prepared) = memory_prepared {
            prepared.publish();
        }
        Ok(child)
    }

    pub fn remove_child(
        &self,
        parent: &Arc<CgroupNode>,
        name: &str,
        expected: &Arc<CgroupNode>,
    ) -> Result<(), SystemError> {
        let _cpuset_guard = super::cpuset::lock();
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(parent) {
            return Err(SystemError::ENOENT);
        }
        let child = parent
            .children
            .read()
            .get(name)
            .cloned()
            .ok_or(SystemError::ENOENT)?;
        if !Arc::ptr_eq(&child, expected) {
            return Err(SystemError::ENOENT);
        }

        // UPDATE (also the cpuset lock) covers fork reservation through task
        // publication. Accounting serializes exit/migration with this check.
        // Zombies no longer populate the directory, although their pids CSS
        // charge survives until pids_release() at final reap.
        let accounting_guard = cgroup_accounting_lock().lock();
        if child.has_children() {
            return Err(SystemError::ENOTEMPTY);
        }
        if child.has_tasks() {
            return Err(SystemError::EBUSY);
        }
        let removed_child = parent.children.write().remove_entry(name);
        let removed = self.all_nodes.lock().remove(&child.id());
        drop(accounting_guard);

        // Open directory FDs may keep this node alive; they do not keep its
        // attachments installed after rmdir. Drop program refs outside locks.
        let empty_state = DeviceBpfState::empty();
        let old_state = core::mem::replace(&mut *child.device_bpf.write(), empty_state);
        drop(_structure_guard);
        super::cpu::retire_node_locked(&child);
        if let Some(generation) = child.replace_memory_generation(None) {
            generation.retire();
        }
        drop(old_state);
        drop(removed);
        drop(removed_child);
        Ok(())
    }

    /// Legacy `BPF_PROG_ATTACH` for `BPF_CGROUP_DEVICE`. All descendants are
    /// prepared before any visible policy is changed.
    pub fn attach_device_program(
        &self,
        node: &Arc<CgroupNode>,
        prog: Arc<BpfProg>,
        flags: u32,
        replace: Option<Arc<BpfProg>>,
    ) -> Result<(), SystemError> {
        if prog.prog_type() != bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE
            || replace
                .as_ref()
                .is_some_and(|old| old.prog_type() != bpf_prog_type::BPF_PROG_TYPE_CGROUP_DEVICE)
        {
            return Err(SystemError::EINVAL);
        }
        let allowed_flags =
            BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI | BPF_F_REPLACE | BPF_DEVICE_F_PREORDER;
        if flags & !allowed_flags != 0
            || flags & BPF_F_ALLOW_OVERRIDE != 0 && flags & BPF_F_ALLOW_MULTI != 0
            || flags & BPF_F_REPLACE != 0 && flags & BPF_F_ALLOW_MULTI == 0
            || (flags & BPF_F_REPLACE != 0) != replace.is_some()
        {
            return Err(SystemError::EINVAL);
        }

        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(node) {
            return Err(SystemError::ENOENT);
        }
        if !Self::hierarchy_allows_device_attach(node) {
            return Err(SystemError::EPERM);
        }

        let current = node.device_bpf.read();
        let mode = flags & (BPF_F_ALLOW_OVERRIDE | BPF_F_ALLOW_MULTI);
        if !current.direct.is_empty() && current.flags != mode {
            return Err(SystemError::EPERM);
        }
        if current.direct.len() >= BPF_CGROUP_MAX_PROGS {
            return Err(SystemError::E2BIG);
        }
        let mut direct = Vec::new();
        direct
            .try_reserve(current.direct.len() + 1)
            .map_err(|_| SystemError::ENOMEM)?;
        direct.extend(current.direct.iter().cloned());
        drop(current);

        if mode & BPF_F_ALLOW_MULTI == 0 {
            let entry = AttachedDeviceProgram { prog, flags };
            if direct.is_empty() {
                direct.push(entry);
            } else {
                direct[0] = entry;
            }
        } else {
            if direct.iter().any(|entry| {
                Arc::ptr_eq(&entry.prog, &prog)
                    && replace.as_ref().is_none_or(|old| !Arc::ptr_eq(old, &prog))
            }) {
                return Err(SystemError::EINVAL);
            }
            let entry = AttachedDeviceProgram { prog, flags };
            if let Some(replace) = replace {
                let old = direct
                    .iter_mut()
                    .find(|candidate| Arc::ptr_eq(&candidate.prog, &replace))
                    .ok_or(SystemError::ENOENT)?;
                *old = entry;
            } else {
                direct.push(entry);
            }
        }

        let mut updates = self.prepare_device_snapshots(node, &direct, mode)?;
        let old_direct = {
            let mut state = node.device_bpf.write();
            state.flags = mode;
            core::mem::replace(&mut state.direct, direct)
        };
        Self::publish_device_snapshots(&mut updates);
        drop(_structure_guard);
        drop(old_direct);
        Ok(())
    }

    pub fn detach_device_program(
        &self,
        node: &Arc<CgroupNode>,
        prog: Option<&Arc<BpfProg>>,
    ) -> Result<(), SystemError> {
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(node) {
            return Err(SystemError::ENOENT);
        }
        let current = node.device_bpf.read();
        if current.direct.is_empty() {
            return Err(SystemError::ENOENT);
        }
        let index = if current.flags & BPF_F_ALLOW_MULTI != 0 {
            let prog = prog.ok_or(SystemError::EINVAL)?;
            current
                .direct
                .iter()
                .position(|entry| Arc::ptr_eq(&entry.prog, prog))
                .ok_or(SystemError::ENOENT)?
        } else {
            // Legacy NONE and OVERRIDE modes ignore a supplied program FD.
            0
        };
        let mut direct = Vec::new();
        direct
            .try_reserve(current.direct.len().saturating_sub(1))
            .map_err(|_| SystemError::ENOMEM)?;
        direct.extend(current.direct.iter().enumerate().filter_map(|(i, entry)| {
            if i == index {
                None
            } else {
                Some(entry.clone())
            }
        }));
        let mode = if direct.is_empty() { 0 } else { current.flags };
        drop(current);

        let mut updates = self.prepare_device_snapshots(node, &direct, mode)?;
        let old_direct = {
            let mut state = node.device_bpf.write();
            state.flags = mode;
            core::mem::replace(&mut state.direct, direct)
        };
        Self::publish_device_snapshots(&mut updates);
        drop(_structure_guard);
        drop(old_direct);
        Ok(())
    }

    /// Return a stable copy of direct/effective IDs and direct per-program
    /// flags. The caller performs all user copies after releasing this lock.
    pub fn query_device_programs(
        &self,
        node: &Arc<CgroupNode>,
        effective: bool,
    ) -> Result<(u32, Vec<u32>, Vec<u32>), SystemError> {
        let _structure_guard = self.structure_lock.lock();
        if !self.is_online(node) {
            return Err(SystemError::ENOENT);
        }
        let state = node.device_bpf.read();
        let programs = if effective {
            state.effective.as_slice()
        } else {
            &[]
        };
        let count = if effective {
            programs.len()
        } else {
            state.direct.len()
        };
        let mut ids = Vec::new();
        let mut attach_flags = Vec::new();
        ids.try_reserve_exact(count)
            .map_err(|_| SystemError::ENOMEM)?;
        if !effective {
            attach_flags
                .try_reserve_exact(count)
                .map_err(|_| SystemError::ENOMEM)?;
        }
        if effective {
            ids.extend(programs.iter().map(|prog| prog.id()));
        } else {
            ids.extend(state.direct.iter().map(|entry| entry.prog.id()));
            attach_flags.resize(count, state.flags);
        }
        Ok((if effective { 0 } else { state.flags }, ids, attach_flags))
    }

    fn hierarchy_allows_device_attach(node: &Arc<CgroupNode>) -> bool {
        let mut parent = node.parent();
        while let Some(ancestor) = parent {
            let state = ancestor.device_bpf.read();
            if state.flags & BPF_F_ALLOW_MULTI != 0 {
                return true;
            }
            if !state.direct.is_empty() {
                return state.flags & BPF_F_ALLOW_OVERRIDE != 0;
            }
            parent = ancestor.parent();
        }
        true
    }

    /// Iterate the Linux effective-chain candidates, retaining the original
    /// per-cgroup FIFO index for the global PREORDER ordering.
    fn visit_effective_device_programs<F>(
        node: &Arc<CgroupNode>,
        changed: &Arc<CgroupNode>,
        direct: &[AttachedDeviceProgram],
        flags: u32,
        mut visit: F,
    ) where
        F: FnMut(usize, usize, &AttachedDeviceProgram),
    {
        let mut current = Some(node.clone());
        let mut count = 0;
        let mut depth = 0;
        while let Some(ancestor) = current {
            let state = ancestor.device_bpf.read();
            let (entries, mode) = if Arc::ptr_eq(&ancestor, changed) {
                (direct, flags)
            } else {
                (state.direct.as_slice(), state.flags)
            };
            if count == 0 || mode & BPF_F_ALLOW_MULTI != 0 {
                for (index, entry) in entries.iter().enumerate() {
                    visit(depth, index, entry);
                }
                count += entries.len();
            }
            current = ancestor.parent();
            depth += 1;
        }
    }

    fn prepare_device_snapshots(
        &self,
        changed: &Arc<CgroupNode>,
        direct: &[AttachedDeviceProgram],
        flags: u32,
    ) -> Result<DeviceSnapshotUpdates, SystemError> {
        let mut nodes = Vec::new();
        nodes.try_reserve(1).map_err(|_| SystemError::ENOMEM)?;
        nodes.push(changed.clone());
        let mut index = 0;
        while index < nodes.len() {
            let current = nodes[index].clone();
            let child_count = current.children.read().len();
            nodes
                .try_reserve(child_count)
                .map_err(|_| SystemError::ENOMEM)?;
            nodes.extend(current.children.read().values().cloned());
            index += 1;
        }

        let mut snapshots = Vec::new();
        snapshots
            .try_reserve_exact(nodes.len())
            .map_err(|_| SystemError::ENOMEM)?;
        for node in nodes {
            let mut preorder_count = 0usize;
            let mut normal_count = 0usize;
            Self::visit_effective_device_programs(&node, changed, direct, flags, |_, _, entry| {
                if entry.flags & BPF_DEVICE_F_PREORDER != 0 {
                    preorder_count += 1;
                } else {
                    normal_count += 1;
                }
            });

            let mut preorder = Vec::new();
            let mut normal = Vec::new();
            preorder
                .try_reserve_exact(preorder_count)
                .map_err(|_| SystemError::ENOMEM)?;
            normal
                .try_reserve_exact(normal_count)
                .map_err(|_| SystemError::ENOMEM)?;
            Self::visit_effective_device_programs(
                &node,
                changed,
                direct,
                flags,
                |depth, local_index, entry| {
                    if entry.flags & BPF_DEVICE_F_PREORDER != 0 {
                        preorder.push((Reverse(depth), local_index, entry.prog.clone()));
                    } else {
                        normal.push(entry.prog.clone());
                    }
                },
            );
            preorder.sort_unstable_by_key(|(depth, index, _)| (*depth, *index));
            let mut effective = Vec::new();
            effective
                .try_reserve_exact(preorder_count + normal_count)
                .map_err(|_| SystemError::ENOMEM)?;
            effective.extend(preorder.into_iter().map(|(_, _, prog)| prog));
            effective.extend(normal);
            snapshots.push((node, Arc::new(effective)));
        }
        Ok(snapshots)
    }

    fn publish_device_snapshots(updates: &mut DeviceSnapshotUpdates) {
        // Swapping leaves all old snapshots in `updates`, to be dropped only
        // after the structure lock is released by the caller.
        for (node, snapshot) in updates {
            core::mem::swap(&mut node.device_bpf.write().effective, snapshot);
        }
    }

    #[allow(dead_code)]
    pub fn find_or_create_path(&self, path: &str) -> Result<Arc<CgroupNode>, SystemError> {
        let rel = normalize_cgroup_abs_path(path)?;
        let mut cur = self.root();

        if rel.is_empty() {
            return Ok(cur);
        }

        for comp in rel.split('/') {
            if comp.is_empty() {
                continue;
            }
            cur = self.create_child(&cur, comp)?;
        }

        Ok(cur)
    }

    #[allow(dead_code)]
    pub fn find_path(&self, path: &str) -> Result<Arc<CgroupNode>, SystemError> {
        let rel = normalize_cgroup_abs_path(path)?;
        let mut cur = self.root();

        if rel.is_empty() {
            return Ok(cur);
        }

        for comp in rel.split('/') {
            if comp.is_empty() {
                continue;
            }
            let next = cur
                .children
                .read()
                .get(comp)
                .cloned()
                .ok_or(SystemError::ENOENT)?;
            cur = next;
        }

        Ok(cur)
    }
}

#[derive(Debug, Clone)]
pub struct TaskCgroupRef {
    node: Arc<CgroupNode>,
}

impl TaskCgroupRef {
    pub fn new(node: Arc<CgroupNode>) -> Self {
        Self { node }
    }

    pub fn node(&self) -> Arc<CgroupNode> {
        self.node.clone()
    }
}

lazy_static! {
    static ref CGROUP_ROOT: Arc<CgroupRoot> = CgroupRoot::new();
    static ref CGROUP_ACCOUNTING_LOCK: SpinLock<()> = SpinLock::new(());
}

pub fn cgroup_root() -> &'static Arc<CgroupRoot> {
    &CGROUP_ROOT
}

pub fn cgroup_root_node() -> Arc<CgroupNode> {
    CGROUP_ROOT.root()
}

pub fn cgroup_accounting_lock() -> &'static SpinLock<()> {
    &CGROUP_ACCOUNTING_LOCK
}

fn cgroup_path_projected_from_view(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    let node_path = cgroup_path_components(node);
    let root_path = cgroup_path_components(view_root);
    let common = cgroup_common_ancestor(node, view_root);
    let common_depth = cgroup_path_components(&common).len();

    let up = root_path.len().saturating_sub(common_depth);
    let down = &node_path[common_depth..];

    if up == 0 && down.is_empty() {
        return "/".to_string();
    }

    let mut parts = Vec::with_capacity(up + down.len());
    for _ in 0..up {
        parts.push("..".to_string());
    }
    parts.extend(down.iter().cloned());

    format!("/{}", parts.join("/"))
}

pub fn cgroup_path_from_view(node: &Arc<CgroupNode>, view_root: &Arc<CgroupNode>) -> String {
    cgroup_path_projected_from_view(node, view_root)
}

pub fn cgroup_common_ancestor(left: &Arc<CgroupNode>, right: &Arc<CgroupNode>) -> Arc<CgroupNode> {
    let mut cur = Some(left.clone());
    while let Some(node) = cur {
        if node.is_ancestor_of(right) {
            return node;
        }
        cur = node.parent();
    }
    cgroup_root_node()
}
//一个已经作为管理节点的node不能同时作为迁移目的地承载普通节点
pub fn cgroup_migrate_vet_dst(dst: &Arc<CgroupNode>) -> Result<(), SystemError> {
    // Linux pids_can_attach() transfers charges without applying pids.max;
    // only creating a new task uses quota admission in cgroup_can_fork_in().
    // Callers hold CGROUP_ACCOUNTING_LOCK. rmdir takes the same lock before
    // removing the node from the online registry, so a successful migration
    // cannot attach a task to a directory which has already been removed.
    if !cgroup_root().is_online(dst) {
        return Err(SystemError::ENOENT);
    }
    if !dst.resource_domain().is_valid_domain() {
        return Err(SystemError::EOPNOTSUPP_OR_ENOTSUP);
    }
    if !dst.subtree_control().is_empty() && !dst.is_threaded() && !dst.can_be_thread_root() {
        return Err(SystemError::EBUSY);
    }
    Ok(())
}
//fork前pids.max检查
pub fn cgroup_can_fork_in(node: &Arc<CgroupNode>, new_tasks: usize) -> Result<(), SystemError> {
    if !cgroup_root().is_online(node) {
        return Err(SystemError::ENOENT);
    }
    let mut cur = Some(node.clone());
    while let Some(cg) = cur {
        if let Some(max) = cg.pids_max() {
            let used = cg.pids_current_count();
            if used.saturating_add(new_tasks) > max {
                cg.inc_pids_events_max();
                return Err(SystemError::EAGAIN_OR_EWOULDBLOCK);
            }
        }
        cur = cg.parent();
    }
    Ok(())
}

#[allow(dead_code)]
pub fn find_or_create_node_by_abs_path(path: &str) -> Result<Arc<CgroupNode>, SystemError> {
    cgroup_root().find_or_create_path(path)
}

#[allow(dead_code)]
pub fn find_node_by_abs_path(path: &str) -> Result<Arc<CgroupNode>, SystemError> {
    cgroup_root().find_path(path)
}

fn cgroup_path_components(node: &Arc<CgroupNode>) -> Vec<String> {
    let mut rev = Vec::new();
    let mut cur = Some(node.clone());

    while let Some(n) = cur {
        if !n.name().is_empty() {
            rev.push(n.name().to_string());
        }
        cur = n.parent();
    }

    rev.reverse();
    rev
}

fn normalize_cgroup_abs_path(path: &str) -> Result<String, SystemError> {
    // 支持两种形式：
    // 1) cgroup v2 路径："/foo/bar"
    // 2) 绝对挂载路径："/sys/fs/cgroup/foo/bar"
    let rel = if let Some(stripped) = path.strip_prefix("/sys/fs/cgroup") {
        stripped
    } else {
        path
    };

    if rel.is_empty() {
        return Ok(String::new());
    }

    if !rel.starts_with('/') {
        return Err(SystemError::EINVAL);
    }

    let mut out = Vec::new();
    //单调栈处理..和.
    for comp in rel.split('/') {
        if comp.is_empty() || comp == "." {
            continue;
        }
        if comp == ".." {
            if out.pop().is_none() {
                return Err(SystemError::EINVAL);
            }
            continue;
        }
        out.push(comp);
    }

    Ok(out.join("/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgroup_path_from_view_same_node_is_root() {
        let root = CgroupRoot::new();
        let node = root.create_child(&root.root(), "same").unwrap();

        assert_eq!(cgroup_path_from_view(&node, &node), "/");
    }

    #[test]
    fn cgroup_path_from_view_descendant_stays_relative() {
        let root = CgroupRoot::new();
        let parent = root.create_child(&root.root(), "parent").unwrap();
        let child = root.create_child(&parent, "child").unwrap();

        assert_eq!(cgroup_path_from_view(&child, &parent), "/child");
    }

    #[test]
    fn cgroup_path_from_view_sibling_uses_parent_segments() {
        let root = CgroupRoot::new();
        let left = root.create_child(&root.root(), "left").unwrap();
        let right = root.create_child(&root.root(), "right").unwrap();

        assert_eq!(cgroup_path_from_view(&right, &left), "/../right");
    }
}
