# PR #1995 最新未解决问题核实与修复计划

本文记录 DragonOS-Community/DragonOS#1995 在 2026-07-09 最新未解决 review threads 的核实结论与修复计划。

## 计划约束

先结合 Linux 代码、问题现象、DragonOS 代码深入研究，再制定 plan；制定后先审查 plan 是否符合 Linux 语义、DragonOS 架构、并发/生命周期不变量、错误路径和边界条件，确认无 workaround、无测试特化、无隐藏坑点后才实施代码变更。

代码变更后，必须再次结合 Linux 代码审查 DragonOS 实现。如果发现语义不一致、架构不合理、边界条件遗漏、并发/生命周期风险或 workaround，必须回到 plan 阶段重新制定修复计划，再继续实施。

Linux 参考代码路径：`/root/code/linux-6.6.139/`。

## 最新未解决 review threads

### 1. SIGKILL 与 OOM victim 标记存在唤醒竞态

- Thread: `PRRT_kwDOGrcMY86PkFSB`
- 位置：`kernel/src/mm/oom.rs:353-355`
- 评论要点：`send_signal_info_to_pcb()` 可能在返回前唤醒目标任务；如果目标在另一 CPU 上进入 exit 或页帧分配路径，而 `oom_mm` 还未写入，就可能看到 `current_is_oom_victim() == false`。

#### 核实结论

问题成立，但不能简单改成“先授 reserve、再发送 SIGKILL”。

Linux 6.6 `mm/oom_kill.c::__oom_kill_process()` 在持有 `task_lock(victim)` 的情况下先 `do_send_sig_info(SIGKILL, ...)`，再 `mark_oom_victim(victim)`。其注释说明：应先发送 `SIGKILL`，再授予 memory reserves，避免仍受用户态控制的 victim 提前耗尽 reserves。

DragonOS 当前实现也在 `victim.with_task_lock_irqsave()` 内执行 `send_sigkill()` 与 `mark_oom_mm()`；并且 exit 中 `replace_user_vm(None)` 也在同一把 `task_lock` 下。但 DragonOS 的 exit 在 detach mm 之前仍会执行 `clear_child_tid`、robust list、vfork completion 等逻辑；`send_signal_info_to_pcb()` 内部会设置 fatal group-exit 并唤醒线程。因此在 `mark_oom_mm()` 之前，被唤醒的目标可能已经进入 fatal exit 的前半段，导致需要 reserve 的分配点短暂看不到 OOM victim 身份。

#### 修复方案

将 `oom_mm` 写入改成“提前记录 metadata、延后生效 reserve”的两阶段语义：

1. 在 `send_oom_sigkill()` 中，仍持有 victim `task_lock`。
2. 在发送 `SIGKILL` 前先调用 `victim.sighand().record_oom_victim_mm(candidate.tgid, &candidate.mm)`，让被唤醒后的 fatal exit 路径可以看到 metadata。
3. 修改 `current_is_oom_victim()`：只有同时满足以下条件才返回 true：
   - 当前任务所属 TGID 匹配 `sighand.oom_tgid`；
   - `sighand.oom_mm` 仍存在；
   - 当前任务已经处于 fatal signal pending、group exit 或 `EXITING` 状态。
4. 如果 SIGKILL 发送失败，清理刚写入的 OOM metadata，避免错误授权。

这样做不改变 Linux “未被 fatal kill 控制前不得消耗 reserve”的安全意图，又消除 signal wakeup 与 metadata 写入之间的窗口。

### 2. inflight victim 可能在页表释放完成前被清掉

- Thread: `PRRT_kwDOGrcMY86PkFSI`
- 位置：`kernel/src/mm/oom.rs:208`
- 评论要点：`clear_released_inflight()` 用“没有任务还指向该 mm”判断 victim 已释放。但 exit 先 `replace_user_vm(None)`，之后才 `unmap_all()` 释放页；另一个 OOM fault 在这个窗口内可能提前清掉 inflight，导致级联误杀。

#### 核实结论

问题成立。

DragonOS exit 路径中：

1. `replace_user_vm(None)` 在 `task_lock` 下执行，任务从 PCB 可见 mm 中摘除；
2. 随后才检查 `last_user` 并执行 `old_vm.write().unmap_all()`；
3. `unmap_all()` 内部通过 `MmuGather::finish()` 完成 TLB flush 与物理页释放；
4. 最后才调用 `note_oom_victim_mm_released(old_vm.id())`。

当前 `begin_selection()`、`wait_for_oom_slot()`、`wait_until_recoverable()` 都会调用 `clear_released_inflight(None)`。在 `replace_user_vm(None)` 与 `unmap_all()` 完成之间，`mm_has_user_tasks()` 会返回 false，从而提前清掉 `OOM_STATE.inflight`。这会让其它 OOM fault 误以为上一 victim 已经释放内存，重新选择新 victim，造成不必要的级联 kill。

#### 修复方案

将 inflight 清理从“扫描任务推断”改成“释放完成通知驱动”，并把全局 inflight 生命周期与 victim reserve 生命周期分离：

1. 移除 `begin_selection()`、`wait_for_oom_slot()`、`wait_until_recoverable()` 中基于 `mm_has_user_tasks()` 的 opportunistic clear。
2. 保留等待逻辑，但只根据 `OOM_STATE.selecting` 与 `OOM_STATE.inflight` 判断是否有正在处理的 victim。
3. 在发送任何 `SIGKILL`、唤醒 victim 之前，先把 `{ generation, tgid, mm_id }` 提交为 `OOM_STATE.inflight`。这样 victim 即使很快完成 `unmap_all()`，`note_oom_victim_mm_released(mm_id)` 也不会早到丢失。
4. 如果 `SIGKILL` 失败，按 `generation + tgid + mm_id` 精确回滚 inflight，并清理刚写入的 OOM metadata。
5. 让 `note_oom_victim_mm_released(mm_id)` 成为 exit path 完成 `unmap_all()` 后的权威释放通知，匹配当前 inflight 的 `mm_id` 时只清掉 `OOM_STATE.inflight` 并唤醒等待者。
6. `note_oom_victim_mm_released(mm_id)` 不清理 `sighand.oom_mm`。Linux 6.6 的 `signal_struct::oom_mm` 生命周期长于 `exit_mm()`，退出后半段仍可能需要 reserve；DragonOS 也应避免在 `exit_files()` 等退出清理之前撤销 victim reserve。
7. `OomVictimState` 不再持有 `Arc<AddressSpace>`，只保存 `generation`、`tgid`、`mm_id`。这样 `OOM_STATE` 不会因为自己的强引用阻止 AddressSpace drop。
8. `notify_mm_drop(mm_id)` 只作为防御性兜底清理 inflight，不作为正常释放完成路径的依据；正常路径必须由 `note_oom_victim_mm_released(mm_id)` 驱动。
9. 删除 `notify_mm_reclaim_progress(mm)` 到 OOM waiters 的清理/唤醒耦合，避免 `MmuGather::flush_mmu_free()` 的部分释放造成无效 `wake_all`。

### 3. 子 agent 评审后的裁决

已启动 8 个子 agent 从 Linux 语义、并发/生命周期、架构职责、性能、安全、测试、错误回滚、回归兼容性角度审查本方案。采纳与裁决如下：

- 采纳：`SIGKILL` 前必须先提交 inflight，否则 victim 的 release notification 可能早到丢失。
- 采纳：`OomVictimState` 不能持有 `Arc<AddressSpace>`，否则 `notify_mm_drop()` 兜底存在循环依赖。
- 采纳：`current_is_oom_victim()` 必须成为唯一 reserve 授权入口；`SigHand` 只提供记录 metadata 的接口，避免其它调用者把 metadata 当授权。
- 采纳：`notify_mm_reclaim_progress()` 不应唤醒 OOM waiters；等待条件不消费 reclaim generation 时，部分释放唤醒只会制造 wake storm。
- 采纳：实际 OOM fault-inject 场景应迁入 dunitest，legacy `/bin/test_oom_killer` 只作为补充烟测。
- 部分采纳：`task_will_free_mem()` 是当前 OOM 语义仍缺失的 Linux 兼容点，但不是这两条最新 unresolved thread 的最小闭环。本文档将其记录为后续改进，不在本次修复中扩展候选选择策略，避免把 EXITING/shared-mm 选择语义与本次状态机竞态修复混在一起。
- 不采纳：为稳定复现 detach 与 `unmap_all()` 之间窗口而新增测试 sysctl/暂停钩子。该做法会把测试控制面引入内核退出路径，属于测试特化风险。本次用结构性状态机修复加 fault-inject dunitest 覆盖可观察行为。

实现后再次启动 3 个子 agent 对实际 diff 做 Linux 语义、并发/错误路径、架构/性能/安全复审，并据此追加修订：

- 采纳：如果原候选 TGID 已 detach，但同一个 mm 还有其它 TGID 仍在使用，应改选仍使用该 mm 的 target 作为 primary victim，不能直接等待自然释放。
- 采纳：如果已经没有可杀任务仍使用该 mm，不建立全局 inflight。execve 等路径可能持有 detached old mm Arc，但没有 exit-mm release 通知；等待该 mm drop 可能形成永久 inflight。
- 采纳：不能用 `resident_pages()==0` 作为释放完成信号。PTE 计数和物理页真正回到 buddy allocator 之间隔着 `MmuGather::flush_mmu_free()`。
- 采纳：dunitest 中 other-process victim 场景不能用固定 `usleep` 判断 victim ready；改用 pipe 同步，并提高 victim `oom_score_adj`，降低时序和候选选择不确定性。
- 采纳：`kill_targets_for_mm()` 必须保留实际仍持有 mm 的 task 作为 signal target，不能只保存 thread-group leader；否则 leader 已 detach、非 leader 仍持有 mm 时会误判无可杀目标。
- 采纳：self-victim fault-inject 测例也应显式提高 `oom_score_adj`，并且 other-process victim 等待必须使用有界 `WNOHANG` 轮询和清理路径，避免测例在异常路径永久挂起。

## 预期代码改动

- `kernel/src/mm/oom.rs`
  - 将 `OomVictimState` 改为只保存 `generation`、`tgid`、`mm_id`。
  - 拆分 inflight 清理逻辑：删除基于 `mm_has_user_tasks()` 的推断清理，新增按 `generation/tgid/mm_id` 回滚和按 `mm_id` release 的精确状态转换。
  - 调整 `send_oom_sigkill()`：在 victim `task_lock` 下确认 victim 仍使用 candidate mm，记录 OOM metadata，提交 inflight，然后发送 `SIGKILL`；失败时精确回滚。
  - 如果原候选已经 detach，优先改选仍使用该 mm 的其它 target；target 列表保存实际 mm holder 而不是只保存 leader。如果没有可杀 target 仍使用该 mm，清掉 selecting 并重选，不对 detached mm 建立 inflight。
  - 调整 `current_is_oom_victim()`，增加 fatal/exiting 门控。
  - 删除或空化 `notify_mm_reclaim_progress()` 的 OOM waiter 唤醒行为。
- `kernel/src/ipc/sighand.rs`
  - 将 `mark_oom_mm()` 重命名为 `record_oom_victim_mm()`，强调它只是 metadata 记录。
  - 增加 `clear_oom_mm_if(tgid, mm_id)`，用于 SIGKILL 失败回滚。
  - 增加只供 `current_is_oom_victim()` 使用的 metadata 匹配接口，不让 reserve 判定散落在 `sighand`。
- `kernel/src/mm/mmu_gather.rs`
  - 移除 `flush_mmu_free()` 中无效的 OOM progress 通知调用。
- `user/apps/tests/dunitest/suites/normal/oom_killer_semantics.cc`
  - 将 fault-inject OOM 场景补进 dunitest：self-victim `SIGKILL`、`/proc/vmstat oom_kill` 增量、other-process victim 后 trigger retry 成功。
  - self-victim 和 other-process victim 场景均显式调高 victim `oom_score_adj`。
  - other-process victim 场景使用 pipe ready 同步，victim 触页完成且调高 `oom_score_adj` 后再启动 trigger，并用有界 `WNOHANG` wait 回收/清理 victim。
  - legacy `/bin/test_oom_killer` 保持为补充 guest smoke test。

## 验证计划

1. `make fmt`
2. `make kernel`
3. `git diff --check`
4. 启动 DragonOS guest，运行：
   - `/bin/test_oom_killer`
   - `cd /opt/tests/dunitest && ./dunitest-runner --bin-dir bin --whitelist whitelist.txt --pattern oom_killer_semantics --timeout-sec 120 --verbose`
5. 复查 PR review threads，确认新问题对应代码已被实质解决。

## 已执行验证

- `make fmt`：通过，包含内核 `cargo fmt` 与 clippy。
- `make kernel`：通过，release kernel 成功链接生成。
- `make -C user/apps/tests/dunitest`：通过，`normal/oom_killer_semantics_test` 成功编译安装。
- `git diff --check`：通过。
- DragonOS guest 中运行 `/opt/tests/dunitest/bin/normal/oom_killer_semantics_test`：10 tests passed，其中包含 3 个 OOM fault-inject 测例。
- DragonOS guest 中运行 `/bin/test_oom_killer`：`test_oom_killer: PASS`。
- 两轮子 agent 评审：
  - 方案阶段：8 个子 agent 覆盖 Linux 语义、并发/生命周期、架构、性能、安全、测试、错误回滚和回归兼容性。
  - 实现阶段：先后复审实际 diff，发现并修正 detached mm 等待、`resident_pages()==0` 误判、leader-detached target 选择、测试 `usleep` 时序依赖和异常路径永久等待等问题。

## 风险与约束

- 不实现伪 reserve 或 busy-spin workaround；页帧分配仍只允许已经 fatal/exiting 的 OOM victim 走额外 retry。
- 不以“没有任务引用 mm”作为释放完成条件；释放完成必须来自 `unmap_all()` 之后或 `InnerAddressSpace::drop()`。
- 不扩大 reserve 授权范围；共享 victim mm 的其它 TGID 仍只收到 SIGKILL，不获得 `oom_mm` entitlement。
- 不在 `OOM_STATE` 锁内扫描全进程，避免锁内长路径和潜在死锁。
- 不把 `EXITING`/`task_will_free_mem()` 语义扩展混入本次竞态修复；该缺口需要单独结合 Linux `task_will_free_mem()`、DragonOS thread-group/shared-mm 状态再设计。
