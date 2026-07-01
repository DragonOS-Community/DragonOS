# EXITING reaper 选择语义根因修复计划

关联评论: <https://github.com/DragonOS-Community/DragonOS/pull/1990#discussion_r3504222162>

本文档针对 PR #1990 中 `is_alive_reparent_target()` 未排除 `EXITING` 线程的问题，
给出 Linux 6.6.139 对照分析、DragonOS 根因定位、修复方案与验证计划。

## 开发约束

> 先结合Linux代码、问题现象、dragonos代码深入研究，再制定plan；制定后先审查plan是否符合Linux语义、DragonOS架构、并发/生命周期不变量、错误路径和边界条件，确认无workaround、无测试特化、无隐藏坑点后才实施代码变更。
>
> 代码变更后，必须再次结合Linux代码审查DragonOS实现。如果发现语义不一致、架构不合理、边界条件遗漏、并发/生命周期风险或workaround，必须回到plan阶段重新制定修复计划，再继续实施。
>
> 所有方案都要参考Linux代码、dragonos代码、深入研究，并且制定正确、完善、无坑点、无workaround、架构合理、功能正确的实现/根因修复计划。
>
> Linux代码在： ~/code/linux-6.6.139/

## 评论问题

评论指出：`ProcessManager::exit()` 进入退出流程早期就调用 `mark_exiting()` 设置 `ProcessFlags::EXITING`，
但 `is_alive_reparent_target()` 只排除了 `exited/zombie/dead`。当同一线程组多个线程并发退出时，
一个已经进入退出流程、但还没有变成 zombie/dead 的线程仍可能被选作 reparent 目标。

如果该线程已经执行过自己的 `adopt_childen()`，随后被收养到它名下的 child 不会再被二次收养，
可能保留指向死亡线程的 `real_parent_pcb/wait_parent_pcb/fork_parent_pcb`，破坏后续 `wait()`、`getppid()`、
`PTRACE_TRACEME` 等父子关系语义。

该问题有效。它不是单纯漏了一个状态判断，而是 reaper 候选的生命周期不变量没有和 Linux 的
`PF_EXITING` 规则对齐。

## Linux 6.6.139 语义基线

参考文件：`~/code/linux-6.6.139/kernel/exit.c`

关键路径：

- `do_exit()`
  - 调用 `exit_signals(tsk)`，该函数设置 `PF_EXITING`。
  - 之后才进入 `exit_notify(tsk, group_dead)`。
- `exit_notify()`
  - 在 `tasklist_lock` 写锁下调用 `forget_original_parent()`。
- `find_alive_thread()`
  - 遍历线程组，只返回 `!(t->flags & PF_EXITING)` 的线程。
- `find_new_reaper()`
  - 优先 `find_alive_thread(father)`。
  - 若没有同组可用线程，再沿祖先查找 child subreaper。
  - 对每个 child subreaper 祖先也调用 `find_alive_thread(reaper)`，只有其线程组中存在非 `PF_EXITING`
    线程时才可作为 reaper。
  - 最后 fallback 到 pid namespace child reaper/init。

因此 Linux 的 reaper 候选条件不是“尚未 zombie/dead”，而是“还没有进入退出流程”。`PF_EXITING`
一旦设置，该 task 就不能再作为新的 reparent 目标。

## DragonOS 当前实现与根因

相关代码：

- `kernel/src/process/mod.rs`
  - `ProcessManager::exit()` 早期调用 `pcb.mark_exiting()`。
  - `mark_exiting()` 设置 `ProcessFlags::EXITING` 并执行内存 fence。
  - `is_alive_reparent_target()` 当前只判断：
    - `!pcb.is_exited()`
    - `!pcb.is_zombie()`
    - `!pcb.is_dead()`
  - `find_alive_thread_reaper()` 使用该 helper 选择同线程组 reaper。
  - `collect_children_for_exit()` 对 `wait_parent_pcb` 的同组活线程 fast path 也使用该 helper。
  - `adopt_childen()` 查找 subreaper 时直接选中 ancestor group leader，没有像 Linux 那样确认该
    subreaper 线程组中存在非 `EXITING` 线程。

根因：

1. DragonOS 的“可作为 reaper 的活线程”定义缺失 `EXITING` 排除条件。
2. 同组 sibling reaper、wait-parent fast path、subreaper ancestor reaper 三类路径没有共享同一个
   Linux-style candidate 选择逻辑。
3. subreaper fallback 直接使用 leader，语义弱于 Linux `find_alive_thread(reaper)`。
4. `adopt_childen()` 当前没有把“收集 children、选择 reaper、批量 reparent”作为一个事务串行化；
   仅在单次 `reparent_child_to()` 内加锁不足以防止并发退出线程在候选检查之后完成自己的收养流程。
5. `fork.rs` 中新 child 的 parent 字段初始化与发布到 parent leader 的 `children` 列表不在同一个关系锁
   临界区内；exit/adopt 可能在 parent 字段已写入但 children list 尚未可见的窗口穿过，遗漏这个新 child。

## 修复目标

1. 已设置 `ProcessFlags::EXITING` 的线程不得再作为 reparent target。
2. 同组线程 reaper、wait-parent fast path、subreaper ancestor reaper 均复用同一个候选判断。
3. child_subreaper 祖先必须像 Linux 一样选择其线程组内的非 `EXITING` 活线程，而不是无条件选择 leader。
4. `fork.rs` 的新 task 发布与 `adopt_childen()` 必须像 Linux `tasklist_lock` 一样，串行化 parent 字段写入、
   `children/thread_group` 链接、PID attach、children 收集、reaper 选择和 reparent 迁移。
5. 不引入基于 errno、sleep 或测试特例的 workaround。
6. 不扩大锁作用域到无关路径；仅覆盖父子关系收养事务。

## 修复方案

### 1. 收紧 `is_alive_reparent_target()`

把 `ProcessFlags::EXITING` 纳入不可收养目标：

```rust
fn is_alive_reparent_target(pcb: &Arc<ProcessControlBlock>) -> bool {
    !pcb.flags().contains(ProcessFlags::EXITING)
        && !pcb.is_exited()
        && !pcb.is_zombie()
        && !pcb.is_dead()
}
```

这样覆盖：

- `find_alive_thread_reaper()` 选择同线程组 sibling reaper。
- `collect_children_for_exit()` 中 `wait_parent_pcb` 仍在同线程组时的直接迁移路径。

该变更对齐 Linux `find_alive_thread()` 的 `!(t->flags & PF_EXITING)` 条件。

### 2. 定义清晰的 alive-thread helper

新增按线程组选择 alive task 的 helper：

```rust
fn find_alive_thread_in_group(pcb: Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>>
```

其行为：

- 从 `ProcessManager::thread_group_tasks_snapshot(pcb)` 遍历该线程组；
- 返回第一个 `is_alive_reparent_target()` 为 true 的线程；
- 不额外理解“当前 exiting”语义，只做 Linux-style alive thread 选择。

保留/调整 father 自身同组 reaper helper：

```rust
fn find_alive_thread_reaper(exiting: &Arc<ProcessControlBlock>) -> Option<Arc<ProcessControlBlock>>
```

其职责：

- 调用 `find_alive_thread_in_group(exiting.clone())` 或等价遍历；
- 额外排除 `Arc::ptr_eq(task, exiting)`；
- 仍复用 `is_alive_reparent_target()`，避免 same-thread-group 与 subreaper 两条路径条件漂移。

### 3. 让 subreaper fallback 也选择 alive thread

然后调整 `adopt_childen()` 中 subreaper 查找：

- cursor 从 `exiting.real_parent_pcb()` 开始，匹配 Linux `father->real_parent`，而不是使用
  `parent_pcb()`。
- 遍历时限制在 exiting 所在 active pid namespace：
  - 记录 `let exiting_ns = exiting.active_pid_ns()`；
  - 若候选 ancestor 在 `exiting_ns` 中不可见，或其 `active_pid_ns().level()` 与 `exiting_ns.level()`
    不一致，则停止向上搜索。
  - 这对应 Linux `task_pid(reaper)->level == ns_level` 的边界；DragonOS 当前没有完全等价的
    `struct pid::level` 迭代接口时，应使用 `task_pid_nr_ns(..., Some(exiting_ns.clone())) != 0`
    和 active pid namespace level 作为近似约束，并在代码注释中说明。
- 命中 `leader.sig_info_irqsave().is_child_subreaper()` 后，不直接 `reaper = leader; break;`
- 改为 `if let Some(alive) = find_alive_thread_in_group(leader.clone()) { reaper = alive; break; }`
- 如果该 subreaper 线程组没有 alive thread，继续向上查找更外层 subreaper/init，匹配 Linux。

### 4. 串行化完整 tasklist-like 事务

Linux 在 `copy_process()` 中持有 `tasklist_lock` 写锁完成 task 对外发布：

1. 设置 `p->real_parent` 和 `exit_signal`；
2. 初始化 TGID/PGID/SID/PID 链接；
3. 继承 `has_child_subreaper`；
4. 对进程执行 `list_add_tail(&p->sibling, &p->real_parent->children)`；
5. 对线程执行 `list_add_tail_rcu(&p->thread_group, &p->group_leader->thread_group)`。

Linux 在 `exit_notify()` 中同样持有 `tasklist_lock` 写锁，`forget_original_parent()` 在同一临界区内完成：

1. 处理 ptrace exit；
2. 选择 child reaper；
3. 判断 children 是否为空；
4. `find_new_reaper()`；
5. 遍历 children 并更新 `real_parent/parent`；
6. 把 children 链表拼到 reaper。

DragonOS 应让 fork 发布阶段与 `adopt_childen()` 使用同一个 `PTRACE_RELATION_LOCK`。

fork 侧的发布阶段应放在 cgroup admission、pidfd 安装、rseq fork 等可失败准备工作之后，并在同一个短临界区内完成：

1. 根据 `CLONE_THREAD`、`CLONE_PARENT`、普通 fork 设置 `parent_pcb/real_parent_pcb/wait_parent_pcb/fork_parent_pcb`；
2. 设置 child 视角 `ppid`；
3. 初始化 TGID/PGID/SID/PID；
4. 继承 `has_child_subreaper`；
5. 对非线程 child 加入 parent 线程组 leader 的 `children` 列表；
6. 对新线程加入 group leader 的 `group_tasks`；
7. 在释放关系锁前完成 `attach_pid(PID)`，保证后续 adopt 通过 children pid 能找到 child。

由于 DragonOS 当前 `wait_candidate_children()` 和 `thread_group_tasks_snapshot()` 读侧没有持有
`PTRACE_RELATION_LOCK`，fork 发布顺序还必须避免读侧看到“已在 children/group_tasks 中、但 PID 尚未
attach”的半发布状态：

- 非线程 child 应先完成 TGID/PGID/SID/PID attach，再把 pid push 到 parent leader 的 `children`。
- 新线程应在 `with_group_exec_check()` closure 内先执行 `attach_pid(PID)`，再 push 到 `group_tasks`；
  TGID/PGID/SID 的 init/attach 仍在 closure 返回后执行，避免在共享 sighand 锁内自死锁。

线程发布需要额外注意锁粒度：`CLONE_THREAD` 共享 sighand，`with_group_exec_check()` 持有 sighand 内部锁。
因此该 closure 只能覆盖 group-exec 状态检查、`task_join_group_stop()` 和 `group_tasks` 链接，不能在 closure
内调用 `init_task_pid(TGID/PGID/SID)` 这类会再次访问同一 sighand 的函数；TGID/PGID/SID 的
init/attach 应在 `with_group_exec_check()` 返回后、但仍在 `PTRACE_RELATION_LOCK` 内完成。

exit 侧的 `adopt_childen()` 应在同一关系锁下完成同类父子关系事务：

1. `collect_children_for_exit(&exiting)`
2. `notify_parent_exit_for_children(&children)`
3. same-thread-group reaper 选择
4. namespace init / subreaper / init fallback 选择
5. 批量 reparent

为避免递归加锁：

- 拆出不加锁的内部 helper：

```rust
fn reparent_child_to_locked(child: &Arc<ProcessControlBlock>, new_parent: &Arc<ProcessControlBlock>)
```

- `reparent_child_to()` 保留公开/跨路径入口，只负责获取 `PTRACE_RELATION_LOCK` 后调用 locked helper。
- `adopt_childen()` 持有 `PTRACE_RELATION_LOCK` 后只调用 `reparent_child_to_locked()`。
- `collect_children_for_exit()` 内部的 wait-parent fast path 也改用 locked helper，或拆成接收 reparent
  回调的结构，保证不会在已持锁状态下再次进入 `reparent_child_to()`。

锁序固定为：

1. `PTRACE_RELATION_LOCK`
2. owner/new parent `children` 锁
3. child parent 字段锁

`mark_exiting()` 不需要持有 `PTRACE_RELATION_LOCK`。原因是：

- 若 B 已经完成自己的 `adopt_childen()`，则 B 必然已经设置 `EXITING`，A 在锁内不会再选择 B。
- 若 A 在锁内检查 B 尚未 `EXITING` 后 B 才设置 `EXITING`，B 的 `adopt_childen()` 会阻塞在同一关系锁，
  等 A 把 child 迁到 B 后再执行收养，从而不会错过二次 reparent。
- 若 fork 侧正在创建 child，parent 字段和 `children/group_tasks` 发布也在同一关系锁内完成，exit/adopt
  不会在“parent 已写入但 children 尚未发布”的半发布状态中扫描 children。

### 5. 不改变 reparent 写入职责

`reparent_child_to()` 已是 parent/real_parent/wait_parent/fork_parent/ppid/children link 的集中迁移入口，
本次通过 locked helper 保留该职责，不在调用点重复写 parent 字段。

本次修复只改变“谁可以作为 reaper”和“fork/adopt 发布事务边界”，不改变底层 reparent 字段写入职责。

### 6. 测试策略

并发退出窗口在用户态不容易确定性卡在 `mark_exiting()` 之后、`adopt_childen()` 之前；不应为了测试而加入
内核调试钩子或测试专用 syscall。测试以外部语义回归为主，源码审查证明核心不变量。

计划增加两个 dunitest：

#### `SubreaperLeaderExitUsesAliveThread`

目标：覆盖 child_subreaper fallback 不应直接选择已退出 leader，而应选择该 subreaper 线程组内 alive thread。

流程：

1. outer gtest fork 一个 subreaper helper。
2. helper 调用 `prctl(PR_SET_CHILD_SUBREAPER, 1)`。
3. helper leader 创建 sibling 线程；sibling 保持存活并通过 pipe 同步。
4. helper leader fork 一个 intermediate process。
5. intermediate fork grandchild 后退出，使 grandchild orphan。
6. helper leader 调用 `SYS_exit`，只退出 leader，sibling 继续存活。
7. sibling 使用 `wait4(intermediate, ..., __WNOTHREAD | WNOHANG)` 轮询到 intermediate 已成为其可等待 child，
   然后释放 intermediate 并用 `wait4(intermediate, __WNOTHREAD)` 回收。
8. sibling 使用 `wait4(grandchild, ..., __WNOTHREAD | WNOHANG)` 轮询直到 grandchild 成为其可等待 child，
   然后释放 grandchild 并 `wait4(..., __WNOTHREAD)` 回收。
9. sibling 通过 pipe 把结果回传给 outer gtest，避免把该用例绑定到 DragonOS 当前 thread-group leader
   wait status 的既有差异上。

该用例直接验证 subreaper 线程组 leader 退出时，fallback 选择 alive sibling，而不是 dying/exited leader。

#### `ConcurrentExitingThreadsDoNotKeepChildOnDyingReaper`

目标：压力覆盖评论中的 concurrent EXITING window。

思路：

1. outer gtest fork helper process，避免测试主体被线程退出影响。
2. helper leader 创建两个 sibling 线程。
3. 线程 A fork 一个 child，child 阻塞在 pipe 上。
4. leader 同步释放线程 A、线程 B 近似同时 `SYS_exit`，扩大并发 exit/reparent 窗口。
5. leader 等两个线程退出后释放 child。
6. leader 使用普通 `wait4(child, ...)` 回收 child。
7. 期望 child 最终可被 leader 或 init/subreaper 语义下的有效父进程回收，不出现 `ECHILD`、挂起或错误 exit status。

该测试是回归压力用例，不声称稳定复现 pre-fix 问题。pre-fix 可能因调度时序不同而通过；真正的修复证据是：

- Linux 对照证明 `EXITING/PF_EXITING` 不可作为 reaper。
- DragonOS 所有 reaper candidate 选择路径都排除 `EXITING`。
- DragonOS fork 发布与 `adopt_childen()` 收养事务在同一关系锁下完成，不会把 child 迁到已经错过 adopt 的
  dying thread，也不会让 exit/adopt 漏掉半发布的新 child。
- 现有 wait/ptrace/reparent 测试保持通过。

### 7. 验证命令

需要执行：

```sh
make fmt
make kernel
make -C user/apps/tests/dunitest bin/normal/wait_rusage_test
./user/apps/tests/dunitest/bin/normal/wait_rusage_test --gtest_filter='WaitRusage.ConcurrentExitingThreadsDoNotKeepChildOnDyingReaper'
./user/apps/tests/dunitest/bin/normal/wait_rusage_test --gtest_filter='WaitRusage.SubreaperLeaderExitUsesAliveThread'
./user/apps/tests/dunitest/bin/normal/wait_rusage_test
make all -j$(nproc)
SKIP_GRUB=1 make write_diskimage
make qemu-nographic
```

在 DragonOS guest 内运行：

```sh
/opt/tests/dunitest/bin/normal/wait_rusage_test
```

目标：完整 `wait_rusage_test` 全部通过。

## 风险与边界

- 选择 `EXITING` 线程作为 reaper 是语义错误；排除它不会降低 Linux 兼容性。
- 如果同线程组所有线程都已 `EXITING`，应 fallback 到 subreaper/init，而不是强行选择其中一个 dying thread。
- child_subreaper 祖先若线程组内也没有 alive thread，应继续向上查找；这比当前直接选 leader 更接近 Linux。
- 不在 `reparent_child_to()` 中检查 `EXITING`，因为该 helper 是执行迁移动作的底层入口；候选合法性应由 reaper 选择逻辑保证。
- `fork.rs` 的 children 链接仍采用当前 DragonOS leader children 模型，和 Linux `real_parent->children`
  不完全一致；本评论把该发布纳入关系锁以关闭并发窗口。后续实现完整 ptrace attach/detach 时仍必须复查该模型。

## 子 agent 评审记录

第一轮评审结论：需要修改。已采纳以下意见：

1. `adopt_childen()` 必须在同一关系锁下完成 children 收集、reaper 选择和批量 reparent；仅锁单次
   `reparent_child_to()` 不足以关闭并发退出窗口。
2. 为避免递归锁，需要拆出 `reparent_child_to_locked()`，公开入口负责加锁，`adopt_childen()` 使用 locked helper。
3. subreaper 查找应从 `real_parent_pcb()` 开始，并显式处理 pid namespace 边界；命中 subreaper 后应选择其线程组内 alive thread。
4. helper 设计应拆成 `find_alive_thread_in_group()` 与 `find_alive_thread_reaper()`，避免调用方自由组合导致条件漂移。
5. 测试计划增加可观察的 subreaper leader 退出但 sibling 存活用例，并保留并发退出压力用例。

第二轮实现后对抗性评审结论：需要修改。已采纳以下意见：

1. `PTRACE_RELATION_LOCK` 只覆盖 adopt/reparent 侧不足以模拟 Linux `tasklist_lock`；fork 侧 parent 字段初始化、
   `children` 链接、`group_tasks` 链接和 PID attach 必须纳入同一关系锁临界区。
2. 文档补充 Linux `copy_process()` 中 `tasklist_lock` 同时保护 `p->real_parent` 与
   `p->real_parent->children` 链接的语义。
3. `SubreaperLeaderExitUsesAliveThread` 去掉固定 sleep，并改用 sibling pipe 结果回传，避免测试误判成
   thread-group leader wait status 的既有差异。

实现验证中通过 GDB 现场取样发现一次自死锁：线程 fork 发布路径在 `with_group_exec_check()` 持有共享
sighand 锁时调用 `init_task_pid(TGID/PGID/SID)`，而 `CLONE_THREAD` 的 child 与 current 共享 sighand。
已修正为：group-exec closure 只执行不会触碰共享 sighand 的 `attach_pid(PID)`、检查和 `group_tasks`
链接；TGID/PGID/SID 初始化与 attach 在 closure 返回后、关系锁释放前执行。

第三轮最终复审结论：需要修改。已采纳以下意见：

1. DragonOS wait/thread-group 读侧目前不持 `PTRACE_RELATION_LOCK`，因此 fork 发布顺序不能先暴露
   `children/group_tasks` 再 attach PID。
2. 非线程 child 调整为先完成 PID attach，再 push 到 parent leader 的 `children`。
3. 新线程调整为在 `with_group_exec_check()` closure 内先 `attach_pid(PID)` 再 push `group_tasks`；
   其余会触碰共享 sighand 的 TGID/PGID/SID 初始化仍留在 closure 外。
