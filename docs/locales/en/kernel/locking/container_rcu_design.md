:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/locking/container_rcu_design.md

- Translation time: 2026-05-16 09:41:49

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS Container-Level RCU Design Document

## Document Purpose

This document is the output of **PR4: Container-Level RCU Design Specialization** in the phased RCU solution.

PR4 does not implement specific containers or modify runtime code. Instead, it categorizes container-like objects to be migrated in DragonOS by semantics, clarifying the appropriate RCU model, lifecycle rules, write-side synchronization methods, and read-side benefits and restrictions for each category.

The design conclusions in this document are based on:

- DragonOS's current general non-preemptive RCU implementation: `kernel/src/rcu/mod.rs`
- Current container forms in DragonOS such as processes, PIDs, procfs, mounts, notifiers, and event chains
- Semantic constraints of PIDs, RCU lists/hlists, and IDR/XArrays in Linux 6.6.21
- Abstraction boundaries of Rust RCU pointers, COW Vectors, and XArrays in Asterinas

This document is not an explanation of RCU principles but an engineering design that must be followed when implementing container-level RCU in subsequent PR5 and beyond.

---

## Overall Conclusions

DragonOS already has the skeleton of a general non-preemptive RCU, but this only solves one thing:

> Old objects removed from the publication point cannot be freed while they may still be accessed by old readers.

It **cannot** automatically make in-place modifications of `HashMap`, `BTreeMap`, `Vec`, and `LinkedList` lock-free safe.

Therefore, container-level RCU must adhere to the following principles:

- The read side protects object lifecycles through `rcu_read_lock()`.
- The write side must still have independent locks for serialization.
- Internal nodes within containers cannot be in-place released, relocated, or rebalanced while readers may traverse them.
- Deletion must first remove the object from the RCU-visible publication point, then wait through a grace period, and finally free the old object.
- The read side cannot carry bare references out of the RCU read-side critical section unless an owned reference is first obtained, such as `Arc<T>`.

Subsequently, pseudo-RCU like the following is not allowed:

```rust
// 错误模型：写侧仍原地修改 HashMap，读侧只套 rcu_read_lock。
let _guard = rcu_read_lock();
let value = raw_hash_map.get(&key);
```

The reason is that RCU can only delay object release but cannot protect bucket array resizing, element relocation, iterator invalidation, and concurrent modification of internal metadata in `HashMap`.

---

## Basic Models

### 1. Snapshot COW Containers

Applicable objects:

- Small tables
- Read-heavy, write-light
- Require consistent snapshots
- Write side can accept O(n) cloning

Typical forms:

- `Arc<Vec<T>>`
- `Arc<BTreeMap<K, V>>`
- `Arc<HashMap<K, V>>`

Write-side process:

1. Acquire writer lock.
2. Clone the current snapshot.
3. Insert, delete, or replace on the new snapshot.
4. Publish the new snapshot via RCU pointer.
5. The old snapshot is delayed dropped through RCU.

Read-side semantics:

- Readers see a complete snapshot.
- Readers may see an older version but will not see a partially updated state.
- Iteration is snapshot-consistent.

Limitations:

- Not suitable for large tables with high-frequency writes.
- Not suitable for hot paths like PID numeric indexing in fork/exit.
- Not suitable for large object graphs requiring in-place modification of individual node states.

### 2. RCU ID Table / XArray-like Indexes

Applicable objects:

- Integer ID to object mapping
- Frequent lookups
- Insertions/deletions need to maintain O(log n) or near O(1)
- Do not want to clone the entire table on every write

Typical objects:

- `PidNamespace::pid_map`
- Future file descriptor tables, device number tables, and other integer-indexable objects

Write-side rules:

- Serialized by a writer lock for structural modifications.
- ID allocation and object publication are completed in two steps.
- Slot pointers are published and cleared using RCU pointers.
- After deleting a slot, the slot-owned reference to the old object is delayed released after the grace period.

Read-side rules:

- Lookup slots by ID under `rcu_read_lock()`.
- After success, an owned reference must be obtained, such as `Arc<T>`, before leaving the read side.
- Bare references within slots must not be carried out of the guard.

Comparison with Linux:

- Linux PID uses IDR within a namespace to store `struct pid *`.
- `find_pid_ns()` can be called under `tasklist_lock` or RCU read-side.
- `free_pid()` is delayed released via `call_rcu()` after being deleted from IDR.

DragonOS should not copy the C-style IDR pointer details but must retain the same publication, deletion, and delayed reclamation semantics.

### 3. Intrusive RCU Lists / Hlists

Applicable objects:

- Nodes naturally belong to the object itself.
- Readers may still traverse to old nodes via next pointers after deletion.
- Require weakly consistent traversal, not necessarily snapshot-consistent.

Typical objects:

- Linux `struct pid::tasks[]`
- Future high-frequency event chains, device lists

Write-side rules:

- Serialized by a writer lock for list modifications.
- `add` uses release to publish next pointers.
- `del` cannot immediately clear next pointers that readers may still use.
- Node releases must be delayed until the grace period.

Read-side semantics:

- Allows seeing nodes before concurrent deletions.
- Allows missing concurrently inserted new nodes.
- Does not promise snapshot consistency.

DragonOS currently does not recommend implementing intrusive lists in the first phase of PR5. Safely expressing intrusive nodes, pinning, ownership, and delayed destruction in Rust is more complex than in C and requires specialized design.

### 4. Retain Existing Locks

Applicable objects:

- Write side is complex and not a performance bottleneck.
- Read side may sleep.
- The container maintains consistency across multiple structures, not just an index.
- Currently lacks SRCU or complete VFS RCU pathwalk.

Retaining locks is not a concession but the correct boundary. RCU should not compromise Linux semantics and object lifecycles for "lock-free reads."

---

## Target Container Decisions

## 1. `ALL_PROCESS`

Current form:

- `static ALL_PROCESS: SpinLock<Option<HashMap<RawPid, Arc<ProcessControlBlock>>>>`
- `ProcessManager::find()` locks to look up the table and clone `Arc<PCB>`
- `add_pcb()`, `release()`, `exchange_tid_and_raw_pids()` in-place modifications to the table

Design conclusion:

**PR4 concludes to retain the existing lock, without RCU-izing.**

Reasons:

- It is a global root PID auxiliary index, not the namespace PID primary index in Linux semantics.
- It is coupled with `exchange_tid_and_raw_pids()`, cgroup accounting, parent-child relationship removal, and exit release paths.
- Changing to snapshot COW would require cloning the entire table on system-wide fork/exit, with uncontrollable costs.
- Incorrect model if `HashMap` is modified for RCU reads.

Future direction:

- Maintain `ALL_PROCESS` as a global management table.
- User-visible PID lookups should gradually converge to `PidNamespace::pid_map` / future `RcuIdTable`.
- If future lockless all-task iteration is needed, a dedicated task list should be created instead of reusing `HashMap`.

## 2. `PidNamespace::pid_map`

Current form:

- `InnerPidNamespace::pid_map: HashMap<RawPid, Arc<Pid>>`
- Co-located with `IdAllocator`, `last_pid`, `dead`, `child_reaper` under `SpinLock<InnerPidNamespace>`
- `find_pid_in_ns()` locks to get + clone
- `free_pid()` releases PID numbers in each namespace and deletes from `pid_map`

Design conclusion:

**Future adoption of a dedicated `RcuIdTable<Arc<Pid>>`, not a COW HashMap.**

Reasons:

- PID lookup is a high-frequency path, and fork/exit is also a hot path; cloning the entire table is inappropriate.
- The corresponding Linux 6.6 model is namespace IDR + RCU, not whole-table COW.
- `RawPid` is an integer index, suitable for slot/radix/xarray-like structures.

Target semantics:

- `alloc_pid_in_ns()` allocates IDs under the namespace writer lock.
- After successful allocation, `Arc<Pid>` is published via RCU slot.
- `find_pid_in_ns()` reads the slot under RCU read-side, clones `Arc<Pid>`, and returns it.
- `release_pid_in_ns()` clears the slot under the writer lock and delays dropping the slot-owned `Arc<Pid>`.
- The circular reference issue between `Pid`'s `numbers` and the namespace should be resolved in PID lifecycle design, not by premature dropping or weakening RCU semantics.

Interface direction:

```rust
pub struct RcuIdTable<T> {
    // 内部结构后续实现，可选择 radix/xarray/分层数组。
}

impl<T: Send + Sync + 'static> RcuIdTable<Arc<T>> {
    pub fn load(&self, id: usize) -> Option<Arc<T>>;
    pub fn store_locked(&self, id: usize, value: Arc<T>);
    pub fn remove_locked(&self, id: usize) -> Option<Arc<T>>;
}
```

Constraints:

- Callers of `store_locked/remove_locked` must hold the upper-level writer lock.
- `load()` enters RCU read-side itself and returns an owned `Arc`.
- Bare slot pointers are not exposed.

## 3. `Pid::tasks`

Current form:

- `tasks: [SpinLock<Vec<Weak<ProcessControlBlock>>>; PidType::PIDTYPE_MAX]`
- `pid_task()` locks to read the first upgradable task
- `tasks_iter()` iterates while holding the lock
- `attach_pid()` pushes weak
- `detach_pid()` retains by deleting weak

Linux comparison:

- Linux uses RCU hlist of `struct pid::tasks[PIDTYPE_MAX]`.
- `attach_pid()` is `hlist_add_head_rcu()` under `tasklist_lock`.
- `detach_pid()` uses `hlist_del_rcu()`.
- `pid_task()` gets the first hlist under RCU.

DragonOS design conclusion:

**First phase adopts snapshot COW Vec, not immediately intrusive hlist.**

Reasons:

- The task list for each PID is usually small.
- Rust intrusive RCU hlist requires additional solutions for pinning, node embedding, duplicate chaining, delayed destruction, and `Weak` cleanup.
- COW Vec sufficiently expresses current PIDTYPE lookup and traversal semantics with lower risk.

Target semantics:

- Each `PidType` corresponds to an RCU-published `Arc<Vec<Weak<PCB>>>`.
- `attach_pid()`/`detach_pid()` clones a small Vec under the writer lock and publishes it.
- `pid_task()` reads the snapshot and upgrades the first surviving task.
- `tasks_iter()` does not return a locked iterator but returns an owned `Vec<Arc<PCB>>` or snapshot iterator.

Limitations:

- Old snapshots may contain already exited Weaks; the read side must upgrade and filter.
- After unregister/detach, readers already started may still see old Weaks, but upgrade failures or state checks will filter them.
- If future PGID/SID large-group traversals have performance issues, intrusive RCU hlist can be separately designed.

## 4. procfs `cached_children`

Current form:

- `ProcDir<Ops>::cached_children: RwSem<BTreeMap<String, Arc<dyn IndexNode>>>`
- `list()` first `populate_children()`, then read-locks to collect keys
- `find()` first reads cache, validates and returns; on miss calls `lookup_child()`

Design conclusion:

**Suitable as one of the first PR5 landing candidates, with a snapshot COW map model.**

Reasons:

- Typically small number of child items.
- Low-frequency writes, mainly lazy loading.
- High-frequency reads, lookups and listings benefit from snapshots.
- Return values are inherently `Arc<dyn IndexNode>`, naturally suitable for read-side owned references.

Target semantics:

- `cached_children` is replaced with `RcuCowMap<String, Arc<dyn IndexNode>>` or equivalent encapsulation.
- `populate_children_from_table()` constructs a new snapshot and publishes it once on the write side.
- `lookup_child_from_table()` misses, constructs an inode, then publishes a new snapshot via the write-side lock.
- `validate_child()` must be retained, especially for dynamic `/proc/<pid>`, `/proc/<pid>/fd`, and other directories.

Prohibitions:

- Dynamic PID directories must not be permanently cached as non-invalidatable nodes.
- Inodes must not be created or complex resources allocated or sleepable paths executed within RCU read-side.
- If dynamic directory creation may sleep, it should be executed outside RCU read-side, with write locks added during publication.

## 5. Mount Namespace / Mount Tree

Current form:

- `MountFS::mountpoints: Mutex<BTreeMap<InodeId, Arc<MountFS>>>`
- `MountList` contains three maps: `mounts`, `mfs2ino`, `ino2mp`
- Mount, umount, bind mount, propagation, and rewrite_paths maintain multiple structures simultaneously

Design conclusion:

**Current phase retains existing locks, without RCU-izing.**

Reasons:

- Mount pathwalk and filesystem lookup may sleep, unsuitable for current non-preemptive RCU.
- Mount propagation requires multi-structure consistency, not just single-pointer publication.
- Linux's mount RCU pathwalk relies on seqlock, mount ref, dentry/inode/path multi-layer protocols, and fallback mechanisms.
- DragonOS currently lacks complete `LOOKUP_RCU` and SRCU support.

Future conditions:

- First implement dual-mode RCU/ref-walk for VFS pathwalk.
- Clarify how mount read-side handles conflicts with rename/umount/propagation and reverts to locked mode.
- Introduce necessary sequence counters or equivalent version validation.

Before these conditions are met, `MountList` or `mountpoints` should not be changed to RCU containers.

## 6. Notifier / Subscription Chain / Event Chain

Current form:

- `NotifierChain` internally is a `Vec<Arc<dyn NotifierBlock<...>>>` sorted by priority
- `AtomicNotifierChain` uses `SpinLock`
- `BlockingNotifierChain` uses `RwLock`

Design conclusion:

**`AtomicNotifierChain` is suitable for COW Vec + RCU; `BlockingNotifierChain` is not migrated for now.**

Reasons:

- Atomic notifiers' read-side calls should not sleep, aligning with non-preemptive RCU.
- Notifier registration/unregistration is low-frequency, call_chain is high-frequency.
- Asterinas's console callback / timer softirq callback uses RCU COW Vec, serving as a Rust reference model.
- Blocking notifiers allow sleeping and should wait for SRCU or continue using locks.

Target semantics:

- `register()` clones the current Vec, inserts by priority, and publishes.
- `unregister()` clones the current Vec, deletes, and publishes.
- `call_chain()` iterates the snapshot under RCU read-side.
- Unregister does not guarantee cancellation of ongoing `call_chain()`; it only guarantees new readers will not see the block.

Constraints:

- Callbacks in `AtomicNotifierChain::call_chain()` must not sleep.
- If callbacks may register/unregister the same chain, reentrancy must be clarified; reentrancy safety is not promised by default.
- `nr_to_call` applies to the current snapshot, not across snapshots.

## 7. Epoll / Fasync / Poll Event Chains

Current form:

- Epoll ready lists and poll epitems use `SpinLock<LinkedList<Arc<EPollItem>>>`
- Fasync uses `Mutex<Vec<Arc<FAsyncItem>>>`
- Most paths involve signals, file owners, socket states, and wakeups

Design conclusion:

**Default retention of existing locks, not a PR5 priority.**

Reasons:

- Event callback paths are more prone to intersect with lock ordering, wakeups, and file lifecycles than ordinary notifiers.
- Fasync currently uses `Mutex`, where read-side may not meet atomic RCU callback conditions.
- Epoll has inner spinlock designs for hardirq-safety, which should not be compromised for RCU-ization to align with existing Linux semantics.

Only consider COW snapshot event subscription tables when it is confirmed that read-side does not sleep, callbacks do not require holding outer blocking locks, and deletion semantics can be weakly consistent.

---

## Recommended PR5 Candidates

PR5 should select containers with low risk, clear benefits, and closed semantics.

Recommended priority:

1. COW Vec + RCU for `AtomicNotifierChain`.  
2. Static COW map for `cached_children` in procfs.  
3. COW Vec for `Pid::tasks`.  

**Not recommended for PR5 to implement directly:**  
- `ALL_PROCESS`  
- `PidNamespace::pid_map`  
- Mount namespace / mount tree  
- Epoll ready list  

**Reason:** These objects either have complex semantics, require additional infrastructure, or the benefits do not outweigh the risks.  

---  

## **Future Public Interface Draft**  

PR4 will not implement interfaces, but subsequent implementations should converge within the following boundaries.  

### **`RcuCowVec<T>`**  

**Applicable to:**  
- Notifiers  
- Small subscription tables  
- Small-scale task lists  

**Required interfaces:**  
```rust
pub struct RcuCowVec<T> {
    // RCU-published Arc<Vec<T>>
}

impl<T: Clone + Send + Sync + 'static> RcuCowVec<T> {
    pub fn snapshot(&self) -> Arc<Vec<T>>;
    pub fn update_locked<F>(&self, f: F)
    where
        F: FnOnce(&mut Vec<T>);
}
```  

**Semantics:**  
- `snapshot()` returns an owned `Arc<Vec<T>>`.  
- `update_locked()` requires the caller to already hold the write-side lock.  
- The old Vec is dropped after the GP.  

### **`RcuCowMap<K, V>`**  

**Applicable to:**  
- Cached children in procfs  
- Small read-heavy, write-light directory tables  

**Required interfaces:**  
```rust
pub struct RcuCowMap<K, V> {
    // RCU-published Arc<BTreeMap<K, V>> or Arc<HashMap<K, V>>
}

impl<K, V> RcuCowMap<K, V>
where
    K: Clone + Ord + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn get(&self, key: &K) -> Option<V>;
    pub fn snapshot(&self) -> Arc<BTreeMap<K, V>>;
    pub fn update_locked<F>(&self, f: F)
    where
        F: FnOnce(&mut BTreeMap<K, V>);
}
```  

**Semantics:**  
- `get()` returns a cloned/owned value, not a borrowed one.  
- `snapshot()` is used for consistent iteration like lists.  
- Dynamic items are still validated by the upper layer.  

### **`RcuIdTable<T>`**  

**Applicable to:**  
- PID namespace ID tables  
- Future integer-indexed object tables  

**Required interfaces:**  
```rust
pub struct RcuIdTable<T> {
    // 分层数组、radix 或 xarray 形态由实现 PR 决定。
}

impl<T: Send + Sync + 'static> RcuIdTable<Arc<T>> {
    pub fn load(&self, id: usize) -> Option<Arc<T>>;
    pub fn store_locked(&self, id: usize, value: Arc<T>);
    pub fn remove_locked(&self, id: usize) -> Option<Arc<T>>;
}
```  

**Semantics:**  
- `load()` enters RCU on the read side and returns an owned `Arc`.  
- `store_locked/remove_locked` requires the upper-layer writer lock.  
- Slot-owned references after deletion must be released with delay.  

---  

## **Testing Requirements**  

Each container-related PR must cover at least the following test categories.  

### **Basic Semantics**  
- Items can be found after insertion.  
- New readers cannot see deleted items.  
- Readers that have already started can safely use old objects.  
- Old objects are released after the GP.  
- Duplicate deletions, empty deletions, and duplicate registrations return correct errors.  

### **Concurrency Stress**  
- High-frequency reads on multi-CPU systems.  
- Writers periodically insert, delete, and replace items.  
- Writers delete objects while readers are iterating continuously.  
- Confirm old objects are fully dropped after `rcu_barrier()`.  

### **Subsystem Regression**  
- **PID:** fork/exit, PID reuse, PID namespace destruction, PGID/SID traversal.  
- **procfs:** `/proc` list/find, `/proc/<pid>` invalidation after exit, `/proc/net` static items.  
- **Notifier:** register/unregister/call_chain order, priority, `nr_to_call`.  
- **Mount:** If touched in the future, must cover bind mount, umount, propagation, pivot_root, mountinfo.  

### **Debug Assertions**  
- No sleepable interfaces may be called in RCU read-side.  
- Container APIs must not expose escapable raw references.  
- Write-side update APIs should check writer lock preconditions in debug mode as much as possible.  

---  

## **Explicit Prohibitions**  
- **Prohibited:** Exposing standard library or `hashbrown` in-place modified maps directly to RCU readers.  
- **Prohibited:** Executing potentially blocking operations (inode creation, memory reclamation waits, user memory copies, filesystem IO) in RCU read-side.  
- **Prohibited:** Prematurely releasing objects that may still be seen by readers just to bypass circular references.  
- **Prohibited:** Allowing `synchronize_rcu()` in RCU read-side.  
- **Prohibited:** Treating `Arc` clones as concurrent protection for container structures themselves.  
- **Prohibited:** Implementing `LOOKUP_RCU` before having a complete pathwalk fallback protocol.  

---  

## **One-Sentence Conclusion**  
DragonOS's container-level RCU should start with small COW containers and dedicated ID indexes, retaining write-side locks and returning owned references on the read side; `ALL_PROCESS` and mount trees are not to be RCU-ized for now, and `PidNamespace::pid_map` should wait until the dedicated `RcuIdTable` design is implemented before migration.
