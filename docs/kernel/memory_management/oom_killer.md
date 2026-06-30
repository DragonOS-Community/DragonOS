# OOM Killer 设计说明

## 1. 设计背景

OOM killer 的目标不是“让一次分配成功”，而是在系统已经无法满足内存请求时，选择一个最可能释放足够内存、且相对合适的用户进程终止它，避免内核在同一个失败路径上无意义重试、死循环或系统级死锁。

DragonOS 当前已经有基础页帧分配器、页缓存回收线程、用户态缺页处理和进程退出释放地址空间的能力，但还没有 Linux 6.6 那种完整的页分配慢速路径、reclaim、memcg、NUMA/cpuset OOM 域、OOM reaper 与内存水位联动。因此当前 OOM killer 的第一阶段定位是：

- 解决用户态合法缺页返回 `VM_FAULT_OOM` 后可能在同一 RIP 上反复缺页的问题；
- 在 x86_64 用户态 page fault 尾部主动进入 `mm::oom`；
- 选择一个用户地址空间对应的 victim，向相关线程组投递 `SIGKILL`；
- 等待 victim 的地址空间释放出现可观察进度，再让触发缺页的任务重试 fault；
- 保留与 Linux OOM 语义一致的评分、不可杀条件、共享 `mm` 处理方向，为后续接入页分配慢速路径打基础。

当前实现重点在 `kernel/src/mm/oom.rs`，并由 `kernel/src/mm/mod.rs` 导出。

## 2. 当前总体路径

用户态缺页 OOM 的主路径如下：

```text
x86_64 #PF
  -> arch/x86_64/mm/fault.rs
  -> PageFaultHandler::handle_mm_fault()
  -> 返回 VM_FAULT_OOM
  -> drop(space_guard)
  -> mm::oom::pagefault_out_of_memory(OomContext)
       -> 串行化 OOM 选择
       -> 扫描进程并选择 victim
       -> 对 victim mm 的所有相关线程组发送 SIGKILL
       -> 等待 mm 释放进度或当前任务被 kill
  -> Retry: 重新执行 fault
  -> CurrentTaskKilled: 处理 pending signal 并返回用户态退出
  -> NoVictim: fatal OOM panic
```

`arch/x86_64/mm/fault.rs` 在处理 `VM_FAULT_OOM` 前会释放 `space_guard`。这是重要的并发边界：OOM 路径会扫描全局进程、投递信号、等待 wait queue，不能持有地址空间写锁、VMA 锁、页表编辑锁或分配器锁进入。

## 3. OOM 可能从哪里来

当前 `VM_FAULT_OOM` 主要来自用户缺页处理中的实际内存分配失败：

- `PageFaultHandler::handle_normal_fault()` 分配中间页表失败；
- `do_anonymous_page()` 为私有匿名页调用 `PageMapper::map()` 失败；
- 共享匿名映射 `shared.get_or_create_page()` 或 `map_phys()` 失败；
- 文件私有映射 COW 时 `copy_page_as_normal()` 失败；
- 写保护 fault 中匿名页或私有文件页复制失败；
- `do_fault_around()` 预分配 PTE 页表失败；
- 测试专用 `mm::oom::should_inject_fault_oom()` 命中。

底层页帧分配由 `LockedFrameAllocator` 包装 buddy allocator。`LockedFrameAllocator::allocate()` 当前只尝试直接从 buddy 分配并返回 `Option`，还没有 Linux `__alloc_pages_slowpath()` 那样同步 reclaim、压缩、重试和最终 `out_of_memory()` 的慢路径。`kernel/src/mm/page.rs` 中的 `page_reclaim` 线程会在空闲页低于阈值时回收页缓存中的可回收文件页，但它不是当前分配失败路径上的同步 OOM 判定器。

因此，当前 DragonOS OOM killer 的触发点是“用户态合法 page fault 泄漏出的 `VM_FAULT_OOM`”，而不是完整的全局页分配 OOM。

## 4. `mm::oom` 的关键数据结构

`OomContext` 描述一次 OOM 触发现场：

- `trigger_pid`：触发 fault 的线程 PID；
- `trigger_tgid`：触发 fault 的线程组 ID；
- `fault_address`：缺页地址；
- `fault_ip`：触发 fault 的指令地址；
- `order`：本次请求阶数，当前用户 fault 路径固定为 `1` 页。

`OomOutcome` 是返回给 arch fault 路径的裁决：

- `Retry`：已经提交 victim，且观察到释放进度，触发者可以重试缺页；
- `CurrentTaskKilled`：当前任务已经收到 fatal signal 或正在退出；
- `NoVictim`：没有可杀 victim 或 OOM 核心无法推进，进入 fatal OOM。

全局状态由 `OOM_STATE: SpinLock<OomState>` 保护：

- `generation`：每次开始选择递增，用来关联一次 victim 提交与等待；
- `selecting`：表示当前有 CPU 正在选择 victim；
- `inflight`：表示已有 victim 正在释放内存，其他 OOM 触发者应等待而不是继续杀更多任务。

`OomVictimState` 记录正在等待的 victim 地址空间：

- `mm_id`：`AddressSpace` 的全局唯一 ID；
- `mm: Weak<AddressSpace>`：弱引用，避免 OOM 状态持有 `mm` 导致释放不了；
- `initial_reclaim_generation`：提交 victim 时的释放进度版本；
- `generation`：对应 OOM 轮次。

等待者使用 `OOM_WAITQ`。测试注入使用 `OOM_FAULT_INJECT`，对应 `/proc/sys/vm/oom_fault_inject`，这是 DragonOS-only 的测试接口，不是 Linux ABI，并且受 `CAP_SYS_ADMIN` 限制。

## 5. victim 选择策略

`select_victim()` 扫描 `ProcessManager::get_all_processes()`，对每个任务：

1. 找到任务的 `user_vm()`，没有用户地址空间的任务不参与；
2. 归并到线程组 leader，按 TGID 去重；
3. 读取 leader 的 `oom_score_adj`；
4. 应用不可杀过滤；
5. 计算 badness 分数；
6. 选择分数最高者。

当前不可杀条件包括：

- PID 0；
- 全局 PID 1；
- `KTHREAD`；
- 已带 `EXITING` 标志；
- 处于 active vfork；
- `oom_score_adj == -1000`。

当前评分公式是：

```text
score = resident_user_pages + oom_score_adj * total_system_pages / 1000
```

其中：

- `resident_user_pages` 来自 `AddressSpace::resident_pages()`，表示当前 `mm` 中 present 用户 PTE 的页数；
- `total_system_pages` 来自 `LockedFrameAllocator.usage().total()`；
- `oom_score_adj` 范围是 `[-1000, 1000]`。

如果分数相同，当前实现优先选择 `resident_pages` 更大的候选；仍相同则选择 TGID 更大的候选。这是 DragonOS 当前的确定性 tie-break，不是 Linux ABI。

与 Linux 6.6 对照：Linux `oom_badness()` 的基线是 `get_mm_rss(mm) + swapents + page-table pages`，再加上 `oom_score_adj * totalpages / 1000`。DragonOS 当前没有 swap，也没有把页表页计入评分，因此评分更接近“RSS + oom_score_adj”的阶段性实现。

## 6. `oom_score_adj`

DragonOS 当前实现 `/proc/[pid]/oom_score_adj`，位置在 `kernel/src/filesystem/procfs/pid/oom_score_adj.rs`。

语义要点：

- 可读写范围是 `[-1000, 1000]`；
- `-1000` 表示 OOM 不可杀；
- 非特权写入不能把分数调低到 `oom_score_adj_min` 以下；
- 拥有 `CAP_SYS_RESOURCE` 的写入会同步更新 `oom_score_adj_min`；
- 对共享同一 `mm` 的其他线程组，写入会传播 `oom_score_adj`，但 active vfork 场景会跳过传播；
- `ProcessManager::lock_oom_score_adj()` 用于序列化相关更新。

这与 Linux `/proc/[pid]/oom_score_adj` 的核心方向一致：`oom_score_adj_min` 用来防止非特权进程绕过管理员设置的最低保护线；共享同一 `mm` 的进程也应保持一致的 OOM 评分倾向。

当前状态：DragonOS 已有 `oom_score_adj`，但 `/proc/[pid]/oom_score` 只读评分展示还不是当前 OOM killer 的核心接口。

## 7. SIGKILL 提交与共享地址空间

选中 victim 后，`send_oom_sigkill()` 不只杀选中 TGID。它会调用 `kill_targets_for_mm()` 扫描所有进程，找出共享 candidate `AddressSpace` 的线程组 leader，并对每个目标以 `PidType::TGID` 投递 `SIGKILL`。

这么做是为了匹配 Linux 的重要语义方向：如果多个线程组共享同一个 `mm`，只杀其中一个线程组可能无法释放该地址空间，甚至会让被 OOM kill 的任务卡在退出路径，等待另一个仍在运行并持有同一 `mm` 的任务。Linux `__oom_kill_process()` 也会向共享 victim `mm` 的其他用户进程发送 `SIGKILL`，但跳过 global init 和 kthread。

DragonOS 当前同样跳过 PID 0、PID 1 和 kthread。提交成功后会打印摘要：

```text
oom-kill: trigger_pid=... trigger_tgid=... victim_tgid=...
          score=... adj=... rss=... order=... addr=... ip=...
```

如果当前触发 OOM 的任务本身就是 victim，或者它共享 victim `mm` 且不是 init/kthread，`pagefault_out_of_memory()` 直接返回 `CurrentTaskKilled`，由 fault 路径处理 pending signal。

## 8. 等待释放进度

DragonOS 不把 `resident_user_pages` 下降本身视作 OOM 释放完成。真正用于唤醒 OOM 等待者的是 `AddressSpace::oom_reclaim_generation()`。

原因是：清 PTE 和实际释放物理页之间隔着 TLB shootdown。只要其他 CPU 还可能通过 stale TLB 访问旧物理页，内核就不能释放该页。`kernel/src/mm/mmu_gather.rs` 维护了顺序：

1. 页表项先被清除；
2. 待释放的 `Arc<Page>` 暂存在 `MmuGather::pending_pages`；
3. `flush_mmu_tlbonly()` 完成本地和远端 TLB shootdown；
4. `flush_mmu_free()` 清空 `pending_pages`，触发页引用释放；
5. 如果确实释放了页，调用 `mm.advance_oom_reclaim_generation()`；
6. 调用 `mm::oom::notify_mm_reclaim_progress(mm)` 唤醒 OOM 等待者。

`victim_has_progress()` 判断：

- `Weak<AddressSpace>` 已无法升级，认为有进展；
- `mm_id` 不匹配，认为旧 `mm` 生命周期已结束；
- `oom_reclaim_generation` 相比提交 victim 时发生变化，认为有进展。

如果 `InnerAddressSpace::drop()` 发生，会先 `unmap_all()`，随后调用 `mm::oom::notify_mm_drop(mm_id)` 清除 inflight 状态并唤醒等待者。

## 9. 与进程退出的关系

OOM killer 通过 `SIGKILL` 促使 victim 走普通退出路径，而不是直接在 OOM 上下文释放别的进程地址空间。

进程退出关键步骤在 `ProcessManager::exit()`：

- 先 `mark_exiting()`；
- 处理 `clear_child_tid`、robust list、vfork completion 等仍可能访问用户地址空间的退出工作；
- 之后执行类似 Linux `exit_mm()` 的逻辑：
  - 将当前 CPU 切到 idle 地址空间；
  - 从 PCB 中 `replace_user_vm(None)`；
  - 从旧 `mm.active_cpus` 中清除当前 CPU；
  - 更新 per-CPU TLB 状态；
- 如果没有其他用户任务仍引用旧 `mm`，调用 `old_vm.write().unmap_all()`；
- drop 旧 `Arc<AddressSpace>`，最终触发 `InnerAddressSpace::drop()` 中的 `notify_mm_drop()`。

这里有两个不变量：

- OOM 路径不能直接释放 victim 的 `mm`，必须让 victim 自己经过退出路径，避免破坏 clear tid、robust futex、文件关闭等语义；
- `user_vm=None` 之后，任务不应再作为 OOM victim 候选，因为它已经没有可通过 OOM kill 释放的用户地址空间。

## 10. 与信号系统的关系

OOM 路径使用 `Signal::oom_fatal_signal_pending()` 判断当前任务是否已经注定退出。该 helper 不只检查线程私有 pending `SIGKILL`，还检查：

- 线程组是否已有 group exit code；
- `sighand.shared_pending` 中是否有 `SIGKILL`；
- 普通 `fatal_signal_pending()`。

这是 DragonOS 当前信号模型下的必要补充：OOM victim 投递使用 `PidType::TGID`，信号会进入线程组共享 pending。如果 OOM 等待者只检查线程级 pending，就可能在已经收到进程级 `SIGKILL` 时继续选择新 victim 或重试 fault，造成过度 kill 或 livelock。

## 11. 并发与生命周期注意点

OOM killer 的关键并发规则如下：

- `OOM_STATE` 只保护 OOM 选择和 inflight 状态，不保护进程生命周期本身；
- 选择 victim 时只保存 `Arc<AddressSpace>` 到候选，提交到 inflight 后降级为 `Weak<AddressSpace>`；
- 等待 OOM slot 或等待释放进度时，不能持有 `OOM_STATE`、`AddressSpace` 写锁、VMA 锁、页表编辑锁或分配器锁；
- `selecting` 防止多个 CPU 同时选择并提交 victim；
- `inflight` 防止在已有 victim 正在释放时过度杀进程；
- 观察到 victim 释放进度后，应清除 `inflight` 并唤醒等待者；
- `notify_mm_reclaim_progress()` 和 `notify_mm_drop()` 都必须能在 victim 生命周期结束边界安全调用；
- `resident_user_pages` 是评分统计，不是释放完成判据；
- `oom_score_adj` 更新需要全局序列化，避免共享 `mm` 传播时出现明显不一致；
- vfork 期间跳过 victim 或跳过共享 `mm` 分数传播，是为了避免父子共享地址空间但生命周期关系特殊时误伤或破坏 Linux 兼容语义。

## 12. fatal OOM

当 `select_victim()` 找不到候选，或者 `SIGKILL` 提交失败且不是简单的 `ESRCH` 竞态，`pagefault_out_of_memory()` 返回 `NoVictim`。x86_64 fault 路径随后触发 panic：

```text
fatal user page-fault OOM: pid=... tgid=... addr=... rip=...
```

这是当前阶段的明确失败策略。Linux 在全局 OOM 且没有 killable process 时也会认为系统可能已经无法前进，并可能 panic。DragonOS 当前还没有 memcg OOM、sysrq OOM、panic_on_oom 等分支，因此 fatal OOM 只表示当前用户 fault OOM 闭环无法推进。

## 13. Linux 6.6 语义对照

Linux 6.6 的 OOM 设计有几个核心点：

- `out_of_memory()` 由全局 `oom_lock` 串行化，避免多个上下文过度 kill；
- 页分配失败通常在 `__alloc_pages_slowpath()` 中完成 reclaim、重试和 `__alloc_pages_may_oom()`；
- `pagefault_out_of_memory()` 主要处理 memcg OOM 和已有 fatal signal；全局 OOM 通常应由分配上下文负责；
- `oom_badness()` 基于 RSS、swap、页表页和 `oom_score_adj`；
- `oom_score_adj == -1000` 保护任务不可被 OOM kill；
- 如果任务正在退出或已有 fatal signal，Linux 倾向让它尽快释放内存，而不是再杀其他任务；
- `__oom_kill_process()` 会处理共享 victim `mm` 的其他用户进程；
- OOM victim 会被 `mark_oom_victim()` 标记，并可能交给 OOM reaper 异步回收；
- `panic_on_oom`、`oom_kill_allocating_task`、memcg、cpuset、mempolicy、NUMA 都影响最终策略。

DragonOS 当前已经对齐的方向：

- OOM 选择全局串行化；
- `oom_score_adj` 范围和 `-1000` 不可杀语义；
- 以 resident pages 作为主要 badness 基线；
- 处理共享 victim `mm` 的线程组；
- 避免在已有 fatal signal 或退出中继续 kill；
- 等待真实释放进度后再重试 fault；
- 对 no victim 走明确 fatal OOM。

当前尚未完整对齐的部分：

- OOM 触发点还未前移到页分配慢速路径；
- 没有 memcg OOM 域；
- 没有 NUMA、cpuset、mempolicy 约束；
- 没有 swap，评分不包含 swapents；
- 评分尚未包含页表页；
- 没有 Linux 风格 `TIF_MEMDIE`、`MMF_OOM_SKIP`、`oom_mm` 标记；
- 没有独立 OOM reaper；
- 没有 `panic_on_oom`、`oom_kill_allocating_task`、`oom_dump_tasks` 等 sysctl；
- `/proc/[pid]/oom_score` 仍需后续补齐。

## 14. 当前实现边界

需要特别说明的当前状态：

- OOM killer 只覆盖 x86_64 用户态 page fault 的 `VM_FAULT_OOM` 路径；
- 内核态访问用户地址失败优先走异常表修复或 panic，不进入用户 OOM kill；
- 普通内核分配、slab 分配、DMA 分配、页缓存分配失败不统一触发 `mm::oom`；
- page reclaim 线程是后台回收机制，不是页分配失败路径上的同步 reclaim；
- `oom_fault_inject` 仅用于测试 OOM fault 闭环，不应被用户态程序依赖；
- `do_swap_page()`、`do_numa_page()` 当前仍未实现，对应 Linux 能力也不应在本文中假设存在；
- fatal OOM 当前直接 panic，是第一阶段比静默重试更安全的失败方式。

## 15. 未来演进方向

后续演进建议按以下顺序推进：

1. 将 OOM 触发点前移到页分配慢速路径

   在 `LockedFrameAllocator` / buddy allocator 上层建立类似 Linux `__alloc_pages_slowpath()` 的同步 reclaim、重试和 OOM 判定流程。用户 page fault 尾部应逐步回到 Linux 风格：只处理 memcg OOM、fatal signal 和“泄漏出的 `VM_FAULT_OOM`”告警/重试。

2. 建立更完整的 reclaim 与 OOM 闭环

   将页缓存 reclaim、dirty writeback、不可回收页判断、分配请求上下文整合到统一慢路径中，而不是只依赖后台线程。

3. 引入 OOM victim 标记

   增加类似 Linux `TIF_MEMDIE`、`oom_mm`、`MMF_OOM_SKIP` 的状态，用于区分“已被 OOM kill、应尽快释放”的任务和普通任务，减少重复 kill，并为 OOM reaper 做准备。

4. 实现 OOM reaper 或等价机制

   在确保不破坏 robust futex、clear_child_tid、vfork 和用户态退出语义的前提下，异步回收 victim 的可回收匿名页，缩短 OOM stall 时间。

5. 完善 badness 统计

   在 `resident_user_pages` 基础上加入页表页、swap、shmem、file/anon 分类统计，并补齐 `/proc/[pid]/oom_score`。

6. 接入 cgroup v2 memory OOM

   未来容器场景需要 memory.max、memory.events、oom_kill、oom_group_kill 等语义。memcg OOM 应有自己的 OOM 域，而不是总是全局杀进程。

7. 支持策略 sysctl

   根据 Linux 语义补齐 `panic_on_oom`、`oom_kill_allocating_task`、`oom_dump_tasks` 等策略项，并明确哪些是兼容 ABI，哪些是 DragonOS 内部调试接口。

8. 补齐 NUMA/cpuset/mempolicy 约束

   当 DragonOS 支持相关资源域后，victim 选择必须限制在真正能为本次失败分配释放内存的候选集合中。

9. 增强诊断输出

   OOM 日志应能输出当前内存状态、候选任务表、victim 的 VM/RSS/页表/oom_score_adj 等信息，便于定位真实内存压力来源。
