:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/locking/rcu_implementation_plan.md

- Translation time: 2026-05-16 09:42:10

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS RCU Implementation and Phased Rollout Plan

## Document Purpose

This document serves to formalize the RCU design in DragonOS, current implementation status, subsequent PR breakdown plan, testing strategy, and risk boundaries into an executable engineering plan.

This is not an "RCU principle primer" but rather an implementation specification tailored to DragonOS's current codebase.

## Current Status

As of the current commit, **PR1 has been implemented**, which includes:

- Added generic non-preemptive RCU infrastructure: `kernel/src/rcu/mod.rs`
- Extended `ProcessControlBlock` to distinguish between:
  - `preempt_count`
  - `rcu_read_depth`
- Integrated quiescent state (QS) advancement points:
  - `__schedule()`
  - Before returning from kernel to user space
  - x86/riscv idle extended quiescent state
  - x86 CPU offlining path
- Implemented:
  - `rcu_read_lock()` / `rcu_read_unlock()`
  - `rcu_dereference()`
  - `rcu_assign_pointer()`
  - `call_rcu()`
  - `synchronize_rcu()`
  - `rcu_barrier()`
  - `rcu_defer_drop()`
- Added independent RCU worker kernel thread `rcu_gp`

**Not yet implemented** are:

- RCU migration for specific subsystems
- Container-level RCU structure modifications
- `SRCU`
- `Tasks RCU`
- `LOOKUP_RCU` pathwalk

In other words, the current kernel now has a "correct generic RCU skeleton," but specific object read paths have not yet been extensively switched to RCU.

---

## Why DragonOS Cannot Directly Adopt Linux Tree RCU

DragonOS currently has:

- `preempt_disable/enable`
- Scheduling entry point `__schedule()`
- Idle threads
- Common exit path from interrupts to user space
- Per-CPU infrastructure
- Kernel thread mechanism

But DragonOS **does not yet have** the mature environment that Linux Tree RCU relies on:

- Complete context tracking
- Complete RCU softirq / nocb / callback segmentation system
- Complete lockdep / PROVE_RCU / stall detector support
- Already RCU-ified tasks, PIDs, VFS, and network core containers

Therefore, the most reasonable approach at present is not to directly replicate Linux 6.6 Tree RCU, but rather:

1. First implement **generic non-preemptive RCU**
2. Use it to support single-pointer publication-type objects
3. Then gradually design container-level RCU solutions
4. Finally consider `SRCU` or stronger RCU variants

This is also why the plan is broken down into multiple PRs, rather than attempting to convert all "places that could potentially use RCU" all at once.

---

## Design Goals

### Objectives

- Align with the basic semantics of Linux non-preemptive generic RCU
- Avoid introducing workaround-style "pseudo RCU"
- Do not base object lifetime safety on fortuitous timing
- Prioritize correctness, then gradually pursue read-path performance benefits
- Reserve clear interfaces for future `SRCU`, container-level RCU, and VFS/network migration

### Non-Objectives

- Not implementing complete Linux Tree RCU at this stage
- Not implementing `SRCU` at this stage
- Not implementing `Tasks RCU` at this stage
- Not implementing `LOOKUP_RCU` at this stage
- Not forcibly converting `HashMap/BTreeMap/Vec` into a "lock-free-looking" structure

---

## Reference Baselines

### Linux 6.6.21

This solution references the following principles from Linux 6.6.21 semantically:

- Non-preemptive RCU read side can be bound to `preempt_disable()`
- Quiescent states can be advanced by context switches, returns to user space, and idle quiescent states
- `call_rcu()` and `synchronize_rcu()` must be guaranteed by a true grace period
- Pointer publication and read-side dereferencing must use explicit memory ordering primitives

### Asterinas

Asterinas offers two points of reference for DragonOS:

- Binding RCU read side to "preemption disabled"
- Using a separate monitor to advance the grace period

However, Asterinas's current approach of "directly executing callbacks at the GP completion point" is not suitable for DragonOS's main path, so DragonOS uses an independent worker to execute callbacks, avoiding pushing destruction/callback tail delays into the scheduling path.

---

## Core Design

## 1. RCU Flavor Selection

DragonOS currently adopts:

- **Generic non-preemptive RCU (non-preemptible RCU)**

Basic semantics:

- `rcu_read_lock()` enters the read-side critical section
- The read-side critical section is non-preemptible
- Sleeping is not allowed in the read side
- The writer waits for all old readers to leave via the grace period

Why not preemptible RCU:

- DragonOS's current `preempt_count`, scheduling, wait queues, and lock semantics are closer to "non-preemptible read side"
- If preemptible RCU were implemented now, it would require synchronous modifications to task state tracking, blocked readers management, and more scheduling details, posing excessive risk

---

## 2. Separation of Read-Side State and `preempt_count`

In the current implementation, `ProcessControlBlock` maintains both:

- `preempt_count`
- `rcu_read_depth`

This is necessary.

The reason for not using only `preempt_count`:

- `preempt_count` is also used by spinlocks, rwlocks, and irqsave
- `preempt_count > 0` alone cannot determine whether it is truly in an RCU read side
- `synchronize_rcu()`, debug assertions, and future `PROVE_RCU` style checks all require explicit RCU nesting levels

Current rules:

- `rcu_read_lock()`:
  - `preempt_disable()`
  - `rcu_read_depth += 1`
- `rcu_read_unlock()`:
  - `rcu_read_depth -= 1`
  - `preempt_enable()`

This ensures:

- The read side will not be scheduled out
- While still being able to distinguish between "just holding a spinlock" and "truly in an RCU read side"

---

## 3. Grace Period Design

DragonOS's current GP design is:

- Global `gp_seq`
- Global `completed_gp_seq`
- Global `requested_gp_seq`
- One `RcuCpuState` per CPU
- One `waiting_cpus` set maintained per GP

### GP Initiation

When the following events occur, a future GP is requested:

- `call_rcu()`
- `synchronize_rcu()`
- `rcu_defer_drop()`

If there is no active GP, a new GP is initiated:

1. `gp_seq += 1`
2. Construct the waiting CPU set
3. Wait for all CPUs that need to participate in this GP to report QS

### Which CPUs Participate in This GP

Current rules:

- Only online CPUs participate
- CPUs already in idle extended quiescent state do not enter the waiting set
- Offline CPUs do not enter the waiting set

### GP Completion

When `waiting_cpus` is empty:

- The current GP is completed
- `completed_gp_seq = gp_seq`
- Transfer the callbacks in `target_gp <= completed_gp_seq` to the ready queue

---

## 4. Quiescent State (QS) Advancement Points

Currently integrated QS advancement points are as follows.

### 4.1 Scheduling Path

Location:

- `kernel/src/sched/mod.rs::__schedule()`

Semantics:

- Being able to execute `__schedule()` indicates that the current task is not in an RCU read side
- Because the read side is bound to `preempt_disable()`, scheduling itself signifies a real QS

This is the most stable and core QS source at present.

### 4.2 Returning to User Space

Location:

- `kernel/src/exception/entry.rs`
- Architecture paths of `arch_switch_to_user()`

Semantics:

- Before returning from the kernel to user space, the current CPU has ended its kernel read-side activity for this round
- This aligns with Linux's treatment of "exiting the kernel execution context" as an event usable for RCU advancement

### 4.3 Idle Extended Quiescent State

Location:

- x86 idle loop
- riscv idle loop

Semantics:

- When a CPU enters idle, it is considered to be in an extended quiescent state for RCU
- The GP does not need to wait for CPUs already in idle EQS when started
- If a CPU transitions to idle during an active GP, it can be immediately removed from the waiting set

### 4.4 CPU Offlining

Location:

- Currently integrated x86 `stop_this_cpu()` path

Semantics:

- An offlined CPU should not block the current GP
- If it is still in `waiting_cpus`, it must be explicitly removed

### Subsequent Optional Advancement Points

If needed to enhance GP convergence speed in the future, consider:

- Additional QS on interrupt exit paths
- More granular syscall/exception common entry point supporting state tracking

But these are not necessary to expand prematurely at this stage.

---

## 5. Callback Model

DragonOS currently adopts:

- Decoupling callback enqueueing from GP advancement
- Decoupling callback execution from the scheduling main path
- Executing callbacks in an independent RCU worker kernel thread

### Implemented Interfaces

- `call_rcu(head, func)`
- `rcu_defer_drop<T>()`
- `rcu_barrier()`

### Why Not "Directly Execute Callbacks Upon GP Completion"

Because callbacks in Rust often imply:

- `drop`
- Container destruction
- Releasing object graphs
- Cleaning up complex resources

These may introduce significant tail latency.

If they were directly inserted into:

- `__schedule()`
- Interrupt exits
- Softirq paths

They would pollute the scheduling and interrupt main paths, which is inappropriate for DragonOS at this stage.

Therefore, the current principle is:

- The main path only advances the GP
- Actual callback execution is handled by the worker

---

## 6. Memory Ordering Model

Current constraints are very clear:

- Publishing new pointers must go through `rcu_assign_pointer()`
- Read-side pointer reading must go through `rcu_dereference()`

In the current implementation:

- `rcu_dereference()` uses `Acquire`
- `rcu_assign_pointer()` uses `Release`

The significance of these constraints is:

- The writer's initialization of object content before publishing the pointer is visible to the reader
- After the reader reads the new pointer, it can see the initialized state of the object

At this stage, subsystems are not allowed to directly use bare `AtomicPtr::store/load` to fabricate RCU semantics.

---

## 7. Debugging and Error Protection

Currently implemented or to be maintained debugging constraints:

- `rcu_read_unlock()` underflow directly asserts
- `__schedule()` encountering `rcu_read_depth != 0` issues warnings/assertions
- `synchronize_rcu()` called in an RCU read side issues warnings/assertions
- Repeated enqueueing of the same `RcuHead` directly panics

Future recommendations for enhancement:

- Stall detector
- `/proc` or debugfs exposing:
  - `gp_seq`
  - `completed_gp_seq`
  - Number of pending callbacks
  - Number of ready callbacks
  - Per-CPU `in_idle_eqs`

---

## Why PRs Are Broken Down This Way

Many places "seem like they could use RCU," but they can actually be divided into two categories:

### Category 1: Single-Pointer Publication Objects

Typical characteristics:

- A field is a `Arc<T>` or equivalent single-object reference
- The write side is a "whole replacement"
- The read side is primarily "reading the current version of the object"

These objects are suitable for priority migration.

### Category 2: Container-Level Concurrent Structures

Typical characteristics:

- `HashMap`
- `BTreeMap`
- `Vec`
- `LinkedList`
- In-place additions, deletions, and modifications
- Interleaved iteration and reclamation

These objects should not be directly lock-free just because "RCU infrastructure is now available."

A clear model must first be decided:

- Snapshot copy-on-write
- Intrusive RCU list/hlist
- Dedicated indexing structures

Therefore, PRs must be broken down; otherwise, it is easy to mess up container-level issues while "just adding the infrastructure."

---

## PR Breakdown Plan

## PR1: RCU Core Infrastructure

### Objective

Make DragonOS a kernel with a "correct generic RCU skeleton."

### Scope

- Add `kernel/src/rcu/mod.rs`
- Add `PCB.rcu_read_depth`
- Add RCU APIs:
  - `rcu_read_lock/unlock`
  - `rcu_dereference`
  - `rcu_assign_pointer`
  - `call_rcu`
  - `synchronize_rcu`
  - `rcu_barrier`
  - `rcu_defer_drop`
- Integrate QS advancement points:
  - Scheduling

  - Returning to user space
  - Idle
  - CPU offlining
- Add independent RCU worker kernel thread

### Current Status

**Completed.**

### Acceptance Criteria

- `make kernel` passes
- No compilation or linking issues in scheduling/returning to user space/idle paths
- Able to support subsequent single-pointer RCU work

---

## PR2: First Batch of RCU for Single-Pointer Objects

### Objective

Migrate the most suitable and easiest-to-get-right batch of "single-object reference fields" to RCU.

### Recommended Objects

- `nsproxy`
- `cred`
- `sighand`

These fields are currently high-read, low-write, whole-replacement-type objects.

### Migration Approach

Taking `RwLock<Arc<T>>` as an example, the migration direction is:

1. The write side still allows deciding "whether to update" based on existing locks
2. Once a new object is to be published:
   - Allocate/construct a new `Arc<T>`
   - Publish using `rcu_assign_pointer()`
3. The old object is not immediately `drop`
   - Instead, it is `rcu_defer_drop(old_obj)`
4. The read side no longer always goes through re-locking
   - Instead, it uses `rcu_read_lock()` + `rcu_dereference()`

### Why Do This Batch First

- They are not in-place modification-type containers
- There are no complex iterator invalidation issues
- Lifecycle boundaries are clear
- They can fastest validate whether the RCU infrastructure is truly usable

### What PR2 Does Not Do

- Does not touch `HashMap/BTreeMap/Vec`
- Does not touch the PID global visibility table
- Does not touch the mount tree
- Does not touch the procfs directory cache tree

### PR2 Acceptance Criteria

- `nsproxy/cred/sighand` read paths no longer rely on re-locking clones
- Old versions are delayed-released after object replacement
- No use-after-free
- Existing process/namespace/signal tests do not regress

---

## PR3: Single-Object Reference Migration in Network Namespaces

### Objective

Use real multi-core read paths to validate the actual benefits and stability of RCU in the network subsystem.

### Recommended Objects

- `default_iface`
- Current loopback reference
- Some "current default single objects" under certain netns

### Why PR3 Is Split Out Separately

The concurrency patterns of network code differ from those of processes/credentials:

- It involves more frequent cross-CPU reads
- It interacts more with NAPI/poll/event wakeups
- It is more likely to expose memory ordering and lifecycle issues

Splitting it into a separate PR has two benefits:

1. Easier attribution when problems arise
2. Prevents mixing regressions of process objects and network objects

### What PR3 Does Not Do

- No changes to `device_list`  
- No changes to `bridge_list`  
- No changes to `netlink_socket_table`  

These are all container-level structures and do not fall under the scope of "single-pointer reference migration."  

### PR3 Acceptance Criteria  

- The network default object read path can operate safely under RCU  
- No dangling references introduced during netns destruction  
- No regression in basic socket functionality  

---  

## PR4: Container-Level RCU Design Specialization  

### Objective  

Do not rush into coding; first, complete the full design of the container-level RCU solution.  

For detailed design, see: [`container_rcu_design.md`](container_rcu_design.md).  

### Objects That Must Be Designed Separately  

- `ALL_PROCESS`  
- `PidNamespace::pid_map`  
- procfs cached children  
- mount propagation tree / mount namespace-related visibility structures  
- subscription chains / event chains / certain list containers  

### Why Direct Modifications Are Not Feasible  

The challenge with these containers is not "whether to lock during reads," but rather:  

- When a node becomes visible  
- When a node becomes invisible  
- When an old version can be reclaimed  
- How to ensure safety during concurrent deletion/replacement during iteration  
- How to handle in-place resize/rebalance  

This is not something that can be solved by simply adding a `rcu_read_lock()`.  

### PR4 Deliverables  

Each type of container must clearly adopt one of the following models:  

- Snapshot copy-on-write  
- Intrusive RCU list/hlist  
- Custom array/index  
- "Retain existing locks, do not RCU-ize"  

And explain:  

- Why this model was chosen  
- How lifecycle is ensured  
- What the write-side cost is  
- Whether the read-side benefits justify it  

### PR4 Acceptance Criteria  

- Each target container has a clear solution  
- Implementers do not need to make ad-hoc decisions on the model  
- An independent design document is formed, clarifying which containers can be RCU-ized and which must retain existing locks  

---  

## PR5: First Container-Level RCU Implementation  

### Objective  

Select the easiest-to-implement and most clearly beneficial container-level object and complete the first real container-level RCU-ization.  

### Recommended Priority  

Prioritize from the following two categories:  

1. Certain procfs cache structures  
2. Notifier / tracepoint / subscription chains  

Not recommended for the first implementation:  

- `ALL_PROCESS`  
- `pid_map`  

Because these have complex semantics, broad regression impact, and strong coupling with process exit sequencing.  

### PR5 Acceptance Criteria  

- At least one container-level structure completes a "non-single-pointer" real RCU-ization  
- Concurrent read/write and reclamation semantics are covered by tests  

---  

## Clear Stance on `ALL_PROCESS` and `pid_map`  

This is the most easily misjudged point, so it is explicitly clarified here.  

### Current Conclusion  

**Do not directly change `ALL_PROCESS` or `pid_map` to "RCU lookup tables" in PR2/PR3.**  

### Reason  

Their current essence is:  

- `HashMap<RawPid, Arc<ProcessControlBlock>>`  
- `HashMap<RawPid, Arc<Pid>>`  

The problem is not "whether the value is an Arc or not," but rather:  

- The container itself is modified in-place  
- Iterator/rehash lifecycle is complex  
- Deletion timing is coupled with process exit paths  

If crudely implemented as:  

- Write-side continues modifying `HashMap`  
- Read-side only adds `rcu_read_lock()`  

Then it is only pseudo-RCU and cannot guarantee safety.  

### Reasonable Direction  

These two types of objects should either:  

- Implement a snapshot COW map  
- Introduce a dedicated index structure suitable for RCU  
- Explicitly retain the existing locking model, not for "lock-free sake"  

---  

## Testing Strategy  

## A. Basic Semantics Testing  

Needs to cover:  

- `rcu_read_lock/unlock` nesting  
- Unlock underflow  
- `synchronize_rcu()` waiting for at least one real GP  
- `call_rcu()` callback executed exactly once  
- `rcu_barrier()` waiting for all historical callbacks to complete  

## B. Concurrency Stress Testing  

Needs to cover:  

- Multiple CPU readers continuously entering the read side  
- Writer periodically replacing objects  
- Delayed reclamation of old objects  
- Object reclamation within callbacks  
- High-frequency switching, idle, and user-mode returns jointly advancing GP  

## C. Regression Testing  

Needs to cover:  

- Process/namespace/signal-related paths  
- Network namespace basic paths  
- Kernel threads, scheduling, and idle basic paths  

## D. Debugging Observability  

Recommended additions:  

- GP sequence number observation  
- Callback queue length observation  
- Observation of whether each CPU is in idle EQS  
- Alerts for GPs that take too long to complete  

---  

## Future Expansion Roadmap  

After completing PR1~PR5, further expansion can proceed as needed:  

### Direction 1: Introduce `SRCU`  

Only worthwhile when DragonOS has substantial real-world demand for "read-side sleep allowance," such as:  

- Certain blocking configuration read paths  
- Sleep-type subscription/callback protection  

### Direction 2: More Complex VFS RCU-ization  

This requires:  

- Holistic coordination of dentry/inode/path/mount structures  
- Complete logic for fallback to ref-walk on failure  

Not something to touch at this stage.  

### Direction 3: Stronger Debugging Capabilities  

For example:  

- Stall detector  
- RCU usage checks similar to lockdep  
- Callback reentrancy/leak detection  

---  

## Recommended Implementation Order  

It is strongly recommended to proceed strictly in the following order:  

1. PR1: Infrastructure  
2. PR2: `nsproxy/cred/sighand`  
3. PR3: Network single-object references  
4. PR4: Container-level design specialization  
5. PR5: First real container-level implementation  

Do not rearrange the order as:  

1. PR1  
2. Directly modify `ALL_PROCESS`  
3. Casually modify `pid_map`  

This will significantly increase the risk of encountering issues.  

---  

## Key RCU-Related Files in the Current Repository  

- `kernel/src/rcu/mod.rs`  
- `kernel/src/process/mod.rs`  
- `kernel/src/sched/mod.rs`  
- `kernel/src/exception/entry.rs`  
- `kernel/src/arch/x86_64/process/mod.rs`  
- `kernel/src/arch/riscv64/process/mod.rs`  
- `kernel/src/arch/x86_64/process/idle.rs`  
- `kernel/src/arch/riscv64/process/idle.rs`  

For subsequent PR2/PR3 work, focus will continue to expand to:  

- `kernel/src/process/namespace/nsproxy.rs`  
- `kernel/src/process/cred.rs`  
- `kernel/src/ipc/sighand.rs`  
- `kernel/src/process/namespace/net_namespace.rs`  

---  

## One-Sentence Summary  

What has been completed so far:  

- **"Making DragonOS a kernel with a correctly architected general RCU framework"**  

The meanings of the upcoming PR2/PR3 are respectively:  

- **PR2: Migrating single-object reference read paths for processes/credentials/signals to RCU**  
- **PR3: Migrating single-object reference read paths in the network namespace to RCU**  

These are not "continuing to supplement infrastructure," but rather "beginning to truly utilize this RCU semantics in specific subsystems."
