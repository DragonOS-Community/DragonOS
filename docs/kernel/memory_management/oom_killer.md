# OOM Killer Design

## 1. Design Background

The goal of the OOM killer is not to “make one allocation succeed”. When the system can no longer satisfy a memory request, it selects a relatively suitable user process that is most likely to free enough memory and terminates it. That avoids pointless retries on the same failure path, infinite loops, or a system-wide deadlock.

DragonOS already has a basic page-frame allocator, a page-cache reclaim thread, user-space page-fault handling, and the ability to free an address space on process exit. It does not yet have Linux 6.6’s full page-allocation slow path, reclaim, memcg, NUMA/cpuset OOM domains, OOM reaper, or watermark-driven interaction. The first-stage OOM killer is therefore scoped to:

- Solve the problem that a legitimate user-space page fault returning `VM_FAULT_OOM` can keep faulting at the same RIP;
- Enter `mm::oom` from the tail of the x86_64 user-space page-fault path;
- Select a victim that owns a user address space and deliver `SIGKILL` to the related thread groups;
- Wait until the victim’s address-space teardown shows observable progress, then let the faulting task retry the fault;
- Keep scoring, unkilleable conditions, and shared-`mm` handling aligned with Linux OOM semantics, so later work can hook the page-allocation slow path.

The current implementation lives mainly in `kernel/src/mm/oom.rs` and is exported from `kernel/src/mm/mod.rs`.

## 2. Current Overall Path

The main user-space page-fault OOM path is:

```text
x86_64 #PF
  -> arch/x86_64/mm/fault.rs
  -> PageFaultHandler::handle_mm_fault()
  -> return VM_FAULT_OOM
  -> drop(space_guard)
  -> mm::oom::pagefault_out_of_memory(OomContext)
       -> serialize OOM selection
       -> scan processes and select a victim
       -> send SIGKILL to all thread groups sharing the victim mm
       -> wait for mm reclaim progress or for the current task to be killed
  -> Retry: re-execute the fault
  -> CurrentTaskKilled: handle the pending signal and return to user space to exit
  -> NoVictim: fatal OOM panic
```

`arch/x86_64/mm/fault.rs` drops `space_guard` before handling `VM_FAULT_OOM`. That is an important concurrency boundary: the OOM path scans the global process list, delivers signals, and waits on a wait queue. It must not enter while holding an address-space write lock, a VMA lock, a page-table editing lock, or an allocator lock.

## 3. Where OOM Can Come From

Today `VM_FAULT_OOM` mainly comes from real allocation failures inside user page-fault handling:

- `PageFaultHandler::handle_normal_fault()` fails to allocate an intermediate page table;
- `do_anonymous_page()` fails when calling `PageMapper::map()` for a private anonymous page;
- A shared anonymous mapping fails in `shared.get_or_create_page()` or `map_phys()`;
- A private file mapping fails in `copy_page_as_normal()` during COW;
- Copying an anonymous page or a private file page fails on a write-protect fault;
- `do_fault_around()` fails to preallocate a PTE page table;
- The test-only `mm::oom::should_inject_fault_oom()` hook hits.

Underlying page-frame allocation is a buddy allocator wrapped by `LockedFrameAllocator`. `LockedFrameAllocator::allocate()` currently only tries a direct buddy allocation and returns `Option`. It does not yet have a Linux-style `__alloc_pages_slowpath()` that does synchronous reclaim, compaction, retries, and a final `out_of_memory()`. The `page_reclaim` thread in `kernel/src/mm/page.rs` reclaims reclaimable file pages from the page cache when free pages fall below a threshold, but it is not a synchronous OOM decision point on the current allocation-failure path.

Therefore the current DragonOS OOM killer is triggered by a `VM_FAULT_OOM` that leaks out of a legitimate user-space page fault, not by a complete global page-allocation OOM.

## 4. Key Data Structures in `mm::oom`

`OomContext` describes one OOM trigger site:

- `trigger_pid`: PID of the thread that triggered the fault;
- `trigger_tgid`: thread-group ID of the thread that triggered the fault;
- `fault_address`: the faulting address;
- `fault_ip`: the instruction address that triggered the fault;
- `order`: the request order; the current user-fault path is fixed to `1` page.

`OomOutcome` is the verdict returned to the arch fault path:

- `Retry`: a victim has been committed and reclaim progress has been observed, so the triggerer may retry the page fault;
- `CurrentTaskKilled`: the current task has already received a fatal signal or is exiting;
- `NoVictim`: there is no killable victim, or the OOM core cannot make progress, so the path enters fatal OOM.

Global state is protected by `OOM_STATE: SpinLock<OomState>`:

- `generation`: incremented each time selection starts; used to associate one victim commit with waiters;
- `selecting`: a CPU is currently selecting a victim;
- `inflight`: a victim is already freeing memory, so other OOM triggerers should wait instead of killing more tasks.

`OomVictimState` records the victim address space being waited on:

- `mm_id`: the globally unique ID of the `AddressSpace`;
- `mm: Weak<AddressSpace>`: a weak reference, so OOM state does not pin `mm` and block its release;
- `initial_reclaim_generation`: the reclaim-progress version at the time the victim was committed;
- `generation`: the corresponding OOM round.

Waiters use `OOM_WAITQ`. Test injection uses `OOM_FAULT_INJECT`, which maps to `/proc/sys/vm/oom_fault_inject`. That is a DragonOS-only test interface, not a Linux ABI, and it is restricted by `CAP_SYS_ADMIN`.

## 5. Victim Selection Policy

`select_victim()` scans `ProcessManager::get_all_processes()` and, for each task:

1. Looks up the task’s `user_vm()`; tasks without a user address space are skipped;
2. Collapses to the thread-group leader and deduplicates by TGID;
3. Reads the leader’s `oom_score_adj`;
4. Applies the unkilleable filters;
5. Computes a badness score;
6. Selects the candidate with the highest score.

Current unkilleable conditions include:

- PID 0;
- Global PID 1;
- `KTHREAD`;
- Already marked `EXITING`;
- In an active vfork;
- `oom_score_adj == -1000`.

The current scoring formula is:

```text
score = resident_user_pages + oom_score_adj * total_system_pages / 1000
```

Where:

- `resident_user_pages` comes from `AddressSpace::resident_pages()` and is the number of present user PTEs in the current `mm`;
- `total_system_pages` comes from `LockedFrameAllocator.usage().total()`;
- `oom_score_adj` is in the range `[-1000, 1000]`.

On a score tie, the current implementation prefers the candidate with more `resident_pages`; if still tied, it prefers the larger TGID. That is DragonOS’s current deterministic tie-break, not a Linux ABI.

Comparison with Linux 6.6: the baseline of Linux `oom_badness()` is `get_mm_rss(mm) + swapents + page-table pages`, plus `oom_score_adj * totalpages / 1000`. DragonOS currently has no swap and does not count page-table pages in the score, so scoring is a staged implementation closer to “RSS + oom_score_adj”.

## 6. `oom_score_adj`

DragonOS currently implements `/proc/[pid]/oom_score_adj` in `kernel/src/filesystem/procfs/pid/oom_score_adj.rs`.

Semantic points:

- The readable and writable range is `[-1000, 1000]`;
- `-1000` means the task is unkilleable by OOM;
- An unprivileged write cannot lower the score below `oom_score_adj_min`;
- A write with `CAP_SYS_RESOURCE` also updates `oom_score_adj_min`;
- A write is propagated to other thread groups that share the same `mm`, but propagation is skipped during an active vfork;
- `ProcessManager::lock_oom_score_adj()` serializes the related updates.

This matches the core direction of Linux `/proc/[pid]/oom_score_adj`: `oom_score_adj_min` prevents an unprivileged process from bypassing the administrator’s minimum protection line, and processes that share the same `mm` should keep a consistent OOM scoring bias.

Current status: DragonOS has `oom_score_adj`, but the read-only `/proc/[pid]/oom_score` display is not yet a core interface of this OOM killer.

## 7. SIGKILL Delivery and Shared Address Spaces

After a victim is selected, `send_oom_sigkill()` does not kill only the selected TGID. It calls `kill_targets_for_mm()` to scan all processes, find thread-group leaders that share the candidate `AddressSpace`, and deliver `SIGKILL` to each target with `PidType::TGID`.

This matches an important Linux semantic: if several thread groups share the same `mm`, killing only one of them may not free that address space, and can even stall the OOM-killed task on the exit path while another still-running task holds the same `mm`. Linux `__oom_kill_process()` also sends `SIGKILL` to other user processes that share the victim `mm`, while skipping global init and kthreads.

DragonOS currently also skips PID 0, PID 1, and kthreads. After a successful commit it prints a summary:

```text
oom-kill: trigger_pid=... trigger_tgid=... victim_tgid=...
          score=... adj=... rss=... order=... addr=... ip=...
```

If the task that triggered OOM is itself the victim, or it shares the victim `mm` and is not init/kthread, `pagefault_out_of_memory()` returns `CurrentTaskKilled` immediately and the fault path handles the pending signal.

## 8. Waiting for Reclaim Progress

DragonOS does not treat a drop in `resident_user_pages` itself as OOM reclaim completion. What actually wakes OOM waiters is `AddressSpace::oom_reclaim_generation()`.

The reason is that clearing a PTE and actually freeing the physical page are separated by a TLB shootdown. As long as another CPU might still access the old physical page through a stale TLB, the kernel cannot free that page. `kernel/src/mm/mmu_gather.rs` maintains this order:

1. Page-table entries are cleared first;
2. The `Arc<Page>` objects to be freed are held in `MmuGather::pending_pages`;
3. `flush_mmu_tlbonly()` completes local and remote TLB shootdowns;
4. `flush_mmu_free()` clears `pending_pages` and drops the page references;
5. If pages were actually freed, `mm.advance_oom_reclaim_generation()` is called;
6. `mm::oom::notify_mm_reclaim_progress(mm)` wakes OOM waiters.

`victim_has_progress()` decides progress as follows:

- The `Weak<AddressSpace>` can no longer be upgraded: treat as progress;
- The `mm_id` does not match: the old `mm` lifetime has ended;
- `oom_reclaim_generation` has changed since the victim was committed: treat as progress.

If `InnerAddressSpace::drop()` runs, it first calls `unmap_all()`, then `mm::oom::notify_mm_drop(mm_id)` to clear the inflight state and wake waiters.

## 9. Relationship with Process Exit

The OOM killer uses `SIGKILL` to push the victim onto the normal exit path. It does not free another process’s address space directly from the OOM context.

The key exit steps are in `ProcessManager::exit()`:

- First `mark_exiting()`;
- Handle `clear_child_tid`, the robust list, vfork completion, and other exit work that may still access the user address space;
- Then run logic similar to Linux `exit_mm()`:
  - Switch the current CPU to the idle address space;
  - `replace_user_vm(None)` on the PCB;
  - Clear the current CPU from the old `mm.active_cpus`;
  - Update per-CPU TLB state;
- If no other user task still references the old `mm`, call `old_vm.write().unmap_all()`;
- Drop the old `Arc<AddressSpace>`, which eventually triggers `notify_mm_drop()` in `InnerAddressSpace::drop()`.

There are two invariants here:

- The OOM path must not free the victim’s `mm` directly. The victim must go through its own exit path so clear tid, robust futex, file close, and related semantics are preserved;
- After `user_vm=None`, the task must not be an OOM victim candidate, because it no longer has a user address space that an OOM kill could free.

## 10. Relationship with the Signal System

The OOM path uses `Signal::oom_fatal_signal_pending()` to decide whether the current task is already destined to exit. That helper does not only check a thread-private pending `SIGKILL`; it also checks:

- Whether the thread group already has a group exit code;
- Whether `sighand.shared_pending` contains `SIGKILL`;
- Ordinary `fatal_signal_pending()`.

This is a necessary addition under DragonOS’s current signal model: OOM victim delivery uses `PidType::TGID`, so the signal goes into the thread-group shared pending set. If an OOM waiter only checked thread-local pending, it could keep selecting a new victim or retrying the fault after a process-level `SIGKILL` had already been received, causing over-kill or a livelock.

## 11. Concurrency and Lifetime Considerations

The OOM killer’s key concurrency rules are:

- `OOM_STATE` only protects OOM selection and inflight state; it does not protect process lifetime itself;
- During victim selection, only an `Arc<AddressSpace>` is stored on the candidate; after commit to inflight it is downgraded to `Weak<AddressSpace>`;
- While waiting for an OOM slot or for reclaim progress, do not hold `OOM_STATE`, an `AddressSpace` write lock, a VMA lock, a page-table editing lock, or an allocator lock;
- `selecting` prevents multiple CPUs from selecting and committing a victim at the same time;
- `inflight` prevents over-killing while a victim is already freeing memory;
- After victim reclaim progress is observed, clear `inflight` and wake waiters;
- Both `notify_mm_reclaim_progress()` and `notify_mm_drop()` must be safe to call at the victim lifetime-end boundary;
- `resident_user_pages` is a scoring statistic, not the completion criterion for reclaim;
- `oom_score_adj` updates need global serialization so shared-`mm` propagation does not become visibly inconsistent;
- Skipping a victim or skipping shared-`mm` score propagation during vfork avoids accidentally killing the wrong task, or breaking Linux-compatible semantics, when parent and child share an address space but have a special lifetime relationship.

## 12. Fatal OOM

When `select_victim()` finds no candidate, or `SIGKILL` delivery fails for a reason other than a simple `ESRCH` race, `pagefault_out_of_memory()` returns `NoVictim`. The x86_64 fault path then panics:

```text
fatal user page-fault OOM: pid=... tgid=... addr=... rip=...
```

That is the explicit failure policy for the current stage. Linux also treats a global OOM with no killable process as a situation where the system may no longer be able to make progress, and it may panic. DragonOS does not yet have memcg OOM, sysrq OOM, `panic_on_oom`, or similar branches, so fatal OOM only means the current user-fault OOM loop cannot advance.

## 13. Linux 6.6 Semantics Comparison

Linux 6.6 OOM design has several core points:

- `out_of_memory()` is serialized by the global `oom_lock` to avoid over-killing from multiple contexts;
- Page-allocation failure usually does reclaim, retries, and `__alloc_pages_may_oom()` inside `__alloc_pages_slowpath()`;
- `pagefault_out_of_memory()` mainly handles memcg OOM and an already-pending fatal signal; global OOM is normally the allocation context’s responsibility;
- `oom_badness()` is based on RSS, swap, page-table pages, and `oom_score_adj`;
- `oom_score_adj == -1000` protects a task from being OOM-killed;
- If a task is already exiting or already has a fatal signal, Linux prefers to let it free memory quickly instead of killing more tasks;
- `__oom_kill_process()` also handles other user processes that share the victim `mm`;
- An OOM victim is marked with `mark_oom_victim()` and may be handed to the OOM reaper for asynchronous reclaim;
- `panic_on_oom`, `oom_kill_allocating_task`, memcg, cpuset, mempolicy, and NUMA all affect the final policy.

Directions DragonOS already aligns with:

- Global serialization of OOM selection;
- The `oom_score_adj` range and the `-1000` unkilleable semantic;
- Resident pages as the primary badness baseline;
- Handling thread groups that share the victim `mm`;
- Avoiding further kills when a fatal signal is already pending or the task is exiting;
- Waiting for real reclaim progress before retrying the fault;
- Taking an explicit fatal-OOM path when there is no victim.

Parts that are not yet fully aligned:

- The OOM trigger has not yet been moved forward onto the page-allocation slow path;
- There is no memcg OOM domain;
- There are no NUMA, cpuset, or mempolicy constraints;
- There is no swap, so scoring does not include swapents;
- Scoring does not yet include page-table pages;
- There are no Linux-style `TIF_MEMDIE`, `MMF_OOM_SKIP`, or `oom_mm` marks;
- There is no standalone OOM reaper;
- There are no `panic_on_oom`, `oom_kill_allocating_task`, `oom_dump_tasks`, or similar sysctls;
- `/proc/[pid]/oom_score` still needs to be completed later.

## 14. Current Implementation Boundaries

The current status that needs to be called out:

- The OOM killer only covers the `VM_FAULT_OOM` path of x86_64 user-space page faults;
- A kernel-mode access to a user address that fails prefers exception-table fixup or panic; it does not enter user OOM kill;
- Ordinary kernel allocations, slab allocations, DMA allocations, and page-cache allocations do not uniformly trigger `mm::oom` on failure;
- The page-reclaim thread is a background reclaim mechanism, not synchronous reclaim on the page-allocation failure path;
- `oom_fault_inject` is only for testing the OOM fault loop and must not be relied on by user-space programs;
- `do_swap_page()` and `do_numa_page()` are still unimplemented; the corresponding Linux capabilities must not be assumed in this document;
- Fatal OOM currently panics directly. For the first stage that is a safer failure mode than silently retrying.

## 15. Future Evolution

Later work should proceed in this order:

1. Move the OOM trigger forward onto the page-allocation slow path

   Build a Linux-like `__alloc_pages_slowpath()` flow above `LockedFrameAllocator` / the buddy allocator, with synchronous reclaim, retries, and an OOM decision. The tail of the user page-fault path should gradually return to the Linux style: handle only memcg OOM, fatal signals, and a warning/retry for a leaked `VM_FAULT_OOM`.

2. Build a more complete reclaim and OOM loop

   Integrate page-cache reclaim, dirty writeback, unreclaimable-page decisions, and allocation-request context into a unified slow path, instead of depending only on a background thread.

3. Introduce OOM victim marks

   Add Linux-like `TIF_MEMDIE`, `oom_mm`, and `MMF_OOM_SKIP` state to distinguish tasks that have already been OOM-killed and should free memory quickly from ordinary tasks. That reduces repeated kills and prepares for an OOM reaper.

4. Implement an OOM reaper or an equivalent mechanism

   Asynchronously reclaim the victim’s reclaimable anonymous pages, without breaking robust futex, `clear_child_tid`, vfork, or user-space exit semantics, so OOM stall time is shorter.

5. Complete badness accounting

   On top of `resident_user_pages`, add page-table pages, swap, shmem, and file/anon classified counters, and fill in `/proc/[pid]/oom_score`.

6. Hook cgroup v2 memory OOM

   Future container workloads need `memory.max`, `memory.events`, `oom_kill`, `oom_group_kill`, and related semantics. memcg OOM should have its own OOM domain instead of always killing globally.

7. Support policy sysctls

   Complete `panic_on_oom`, `oom_kill_allocating_task`, `oom_dump_tasks`, and similar policy knobs according to Linux semantics, and make clear which are compatibility ABI and which are DragonOS-internal debug interfaces.

8. Add NUMA/cpuset/mempolicy constraints

   Once DragonOS supports those resource domains, victim selection must be limited to the candidate set that can actually free memory for the failed allocation.

9. Improve diagnostic output

   OOM logs should print the current memory state, a table of candidate tasks, and the victim’s VM/RSS/page-table/`oom_score_adj` information, so the real source of memory pressure can be located.
