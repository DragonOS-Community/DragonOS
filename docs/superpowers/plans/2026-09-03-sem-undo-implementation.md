# SEM_UNDO Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use `superpowers:subagent-driven-development` (recommended) or `superpowers:executing-plans` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 在 DragonOS 的 System V semaphore 实现中，以 Linux 6.6 生命周期语义实现 `SEM_UNDO`，同时保证立即提交、排队重试、控制操作、fork/clone、namespace 切换和 task exit 的记录与回放均具备失败原子性。

**Architecture:** 每个 PCB 持有一个显式的 `SemUndoAttachment` slot；attachment 持有 `Arc<SemUndoGroup>`，group 绑定唯一 `Weak<IpcNamespace>`，并以完整、含 generation 的 `SemId` 保存每个 semaphore-set 的 adjustment slice。每个 IPC namespace 的 `SemManager` 用 `Arc<Vec<Weak<SemUndoGroup>>>` registry snapshot 枚举仍活跃的 group；排队操作以不计入 task owner 数的强 `Arc<SemUndoGroup>` observer 固定原调用者。操作仍由既有 `IpcNamespace.sem: SpinLock<SemManager>` 串行化；本计划不引入 per-set lock、全局 namespace 重构或新的用户 ABI。

**Tech Stack:** Rust `no_std` kernel、`alloc::{Arc, Weak, Vec, Box}`、DragonOS `SpinLock`、System V semaphore syscalls、GoogleTest dunitest、QEMU dunitest runner。

## Global Constraints

- 只在最终提交移除 [`kernel/src/ipc/sem.rs:683-688`](kernel/src/ipc/sem.rs#L683-L688) 的 `SEM_UNDO -> ENOSYS` 门禁；此前所有 `SEM_UNDO` syscall 仍必须返回 `ENOSYS`。
- PCB `sem_undo` slot lock 不得与其他 SEM_UNDO 锁嵌套；跨对象方向固定为 `IpcNamespace.sem` manager lock → `SemUndoGroup.inner` → queue-entry 的 `undo_record` / `scratch` / `status` 锁。禁止任何 group → manager 反向获取。
- 禁止在持有 `ipcns.sem` manager spinlock 时动态分配：不得保留 [`simulate_semop()` 的 `HashMap::with_capacity()`，sem.rs:539-580](kernel/src/ipc/sem.rs#L539-L580)、[`SemQueueEntry::new()` 的 `to_vec()`，sem.rs:264-276](kernel/src/ipc/sem.rs#L264-L276) 或 blocked 分支的 `Arc::new()`（sem.rs:702-735）。
- 所有用户触发的增长都用 `Arc::try_new`、`Vec::try_reserve_exact` 等 fallible API，并映射为 `SystemError::ENOMEM`；任何 allocation error 均不得改变 semval、semadj、queue、record 或 registry。
- record key 必须是完整 `SemId`，不得使用低 index；`adjustments.len()` 必须严格等于目标 `KernelSemSet.sems.len()`。
- `SEMUME`、`SEMMNU` 只维持 [`PosixSemInfo::new()`](kernel/src/ipc/sem.rs#L210-L230) 的报告兼容，绝不参与 admission；操作数上限继续使用全局 `SEMOPM = 500`。
- 维持一个 namespace 一把 manager lock 的既有结构；明确**不实现 per-set lock**。
- 不能把 namespace/fork 全面 publication 重构作为 SEM_UNDO 主体；仅完成“child owner token 在任何 child publication 前安装、任何后续可失败路径先回滚 publication 再撤销 owner”的最小安全协议。
- 所有 kernel 内部测试放在相关模块的 `#[cfg(test)] mod tests`；当前 kernel `Cargo.toml` 的 library 是 `staticlib`，根 `kernel/Makefile:test` 只跑 workspace crates 且排除 `dragonos_kernel`，因此新增的 kernel `#[test]` 需使用项目实际可运行的内核测试 target，不能把 `make -C kernel test` 误报为这些测试的执行证据。
- dunitest 源文件已存在于 [`user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc`](user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc)，且 whitelist 已含 `normal/sysv_sem_semantics`（`whitelist.txt:37`）；无需新建 CMake 注册项。

---

## 实际落点与职责

| 文件 | 变更职责 |
|---|---|
| [`kernel/src/ipc/sem_undo.rs`](kernel/src/ipc/sem_undo.rs) | 新模块：attachment、group、record、owner 计数、record/registry prepare、SETVAL/SETALL/RMID cleanup、最后 owner replay 的可测试原语。 |
| [`kernel/src/ipc/mod.rs:1-12`](kernel/src/ipc/mod.rs#L1-L12) | 导出 `sem_undo`。 |
| [`kernel/src/ipc/sem.rs:6-1008`](kernel/src/ipc/sem.rs) | registry 字段；无分配 simulation/commit；queue 捕获 group；SETVAL/SETALL/RMID cleanup；semtimedop 最终接入。 |
| [`kernel/src/process/task.rs:59-237, 325-407, 1581-1594`](kernel/src/process/task.rs) | PCB `SpinLock<Option<SemUndoAttachment>>` 与语义 API。 |
| [`kernel/src/process/fork.rs:575-1100`](kernel/src/process/fork.rs) | `CLONE_NEWIPC|CLONE_SYSVSEM` 早拒绝；普通 fork 不继承；显式 SYSVSEM 共享、child attachment guard、最小 rollback protocol。 |
| [`kernel/src/process/manager/exit.rs:450-488, 662-704`](kernel/src/process/manager/exit.rs) | robust-list 后、`exit_mm` 前调用唯一 detach/replay；reap 只断言 slot 已空。 |
| [`kernel/src/process/namespace/nsproxy.rs:294-343`](kernel/src/process/namespace/nsproxy.rs) | 将 IPC detach 作为 namespace install 的明确输入；prepare 完成后、安装新 nsproxy 前同步 detach。 |
| [`kernel/src/process/namespace/unshare.rs:19-53`](kernel/src/process/namespace/unshare.rs) | `CLONE_SYSVSEM` 和 `CLONE_NEWIPC` 设置 detach 意图，并接入统一 install helper。 |
| [`kernel/src/process/namespace/setns.rs:96-267`](kernel/src/process/namespace/setns.rs) | pidfd / namespace-fd 两条路径均传递 `detach_sysvsem`，same-Arc IPC setns 仍 detach。 |
| [`kernel/src/process/namespace/ipc_namespace.rs:17-64`](kernel/src/process/namespace/ipc_namespace.rs) | 仅增加已审计的不变量断言，或在无法证明时加明确调用上下文的 `destroy_all()`；禁止在 `Drop` 取 manager lock。 |
| [`kernel/src/process/pid.rs:630-681`](kernel/src/process/pid.rs) | 如 replay 需要以 group namespace 可见的 TGID 更新 `sempid`，将现有私有 `task_pid_nr_ns(PidType::TGID, ...)` 以最小方式开放给 process/IPС 内部调用者；不能用 raw PID 或 `task_pid_vnr()` 替代。 |
| [`user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc:41-226, 706-714`](user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc) | 删除 `SemUndoRejected`，改为公开 ABI SEM_UNDO 语义测试。 |
| [`user/apps/tests/dunitest/Makefile:24-105`](user/apps/tests/dunitest/Makefile) | 不应修改；已有 `wildcard suites/*/*.cc` 自动构建。 |
| [`user/apps/tests/dunitest/whitelist.txt:36-38`](user/apps/tests/dunitest/whitelist.txt#L36-L38) | 不应修改；已有 test binary 白名单。 |

---

### Task 1: 固化 SEM_UNDO 所有权模型和 PCB slot，但保持 ABI 关闭

**Files:**
- Create: `kernel/src/ipc/sem_undo.rs`
- Modify: `kernel/src/ipc/mod.rs:1-12`
- Modify: `kernel/src/process/task.rs:5-56, 59-237, 325-407`
- Modify: `kernel/src/ipc/sem.rs:388-418`
- Test: `kernel/src/ipc/sem_undo.rs` 的 `#[cfg(test)] mod tests`
- Test: `kernel/src/ipc/sem.rs:930-1008`

**Interfaces:**
- Produces:
  ```rust
  pub struct SemUndoAttachment { /* group: Arc<SemUndoGroup> */ }

  pub struct SemUndoGroup {
      /* ipc_ns: Weak<IpcNamespace>,
         inner: SpinLock<SemUndoGroupState> */
  }

  pub(crate) struct UnpublishedSemUndoAttachmentGuard { /* fork-local */ }

  impl ProcessControlBlock {
      pub fn sem_undo_group(&self) -> Option<Arc<SemUndoGroup>>;
      pub fn ensure_sem_undo_group(
          &self,
          ipc_ns: &Arc<IpcNamespace>,
      ) -> Result<Arc<SemUndoGroup>, SystemError>;
      pub fn take_sem_undo_attachment(&self) -> Option<SemUndoAttachment>;
  }
  ```
- Consumes: `IpcNamespace` 的 `Arc`/`Weak`（[`ipc_namespace.rs:17-64`](kernel/src/process/namespace/ipc_namespace.rs#L17-L64)）、`SpinLock`、`SystemError::ENOMEM`。
- Does not produce any user-visible SEM_UNDO behavior in this task.

- [ ] **Step 1: 写失败的 ownership 单测。**

  在新模块 `sem_undo.rs` 的 `#[cfg(test)]` 中先覆盖以下不可变行为：

  ```rust
  #[test]
  fn observer_arc_does_not_change_task_owner_count() {
      let group = SemUndoGroup::new_for_test();
      let attachment = SemUndoAttachment::new_for_test(group.clone());
      let observer = attachment.group_for_test();
      assert_eq!(group.task_owners_for_test(), 1);
      drop(observer);
      assert_eq!(group.task_owners_for_test(), 1);
  }

  #[test]
  fn attachment_is_taken_once_and_drop_never_replays() {
      let attachment = SemUndoAttachment::new_for_test(SemUndoGroup::new_for_test());
      let mut slot = Some(attachment);
      assert!(slot.take().is_some());
      assert!(slot.take().is_none());
  }

  #[test]
  fn group_rejects_different_ipc_namespace() {
      let group = SemUndoGroup::new_for_test_bound_to_first_namespace();
      assert_eq!(
          group.verify_ipc_ns_for_test(second_test_ipc_ns()),
          Err(SystemError::EINVAL)
      );
  }
  ```

  测试 hook 只能在 `cfg(test)` 存在；产品路径不允许公开 fake namespace 构造器或 failpoint ABI。

- [ ] **Step 2: 运行并确认测试失败。**

  Run（选择 DragonOS 实际 kernel-test 编译命令，以下为必须执行的 direct target）：

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib ipc::sem_undo::tests --target x86_64-unknown-linux-gnu
  ```

  Expected: 编译失败，原因是 `ipc::sem_undo`、`SemUndoGroup` 和测试 helper 尚不存在。

  同时记录基线门禁：

  ```bash
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: 仍能看到 `return Err(SystemError::ENOSYS);`。

- [ ] **Step 3: 实现最小数据模型和 PCB slot。**

  在 `sem_undo.rs` 实现且只实现以下所有权基础：

  ```rust
  pub struct SemUndoAttachment {
      group: Arc<SemUndoGroup>,
  }

  pub struct SemUndoGroup {
      ipc_ns: Weak<IpcNamespace>,
      inner: SpinLock<SemUndoGroupState>,
  }

  struct SemUndoGroupState {
      task_owners: usize,
      records: Vec<SemUndoRecord>,
  }

  struct SemUndoRecord {
      semid: SemId,
      adjustments: Box<[i16]>,
  }
  ```

  具体规则：

  1. `SemUndoAttachment` 不派生 `Clone`。
  2. `SemUndoGroup::new(ipc_ns: &Arc<IpcNamespace>) -> Result<Arc<Self>, SystemError>` 使用 `Arc::try_new`，初始 `task_owners = 1`，`records = Vec::new()`。
  3. PCB 新字段为：
     ```rust
     pub(super) sem_undo: SpinLock<Option<SemUndoAttachment>>,
     ```
     在 [`ProcessControlBlock::do_create_pcb()` 的 struct literal，task.rs:325-397](kernel/src/process/task.rs#L325-L397) 初始化为 `SpinLock::new(None)`。
  4. `sem_undo_group()` 只短暂获取 PCB slot lock、clone `Arc` 后释放；不能返回 attachment。
  5. `ensure_sem_undo_group()`：slot lock 检查为空后释放；锁外 `SemUndoGroup::new()`；再次取得 slot lock，已有 group 胜出时丢弃 candidate，否则安装 `SemUndoAttachment`。不嵌套 slot lock 与 group/manager lock。
  6. `take_sem_undo_attachment()` 仅 `self.sem_undo.lock_irqsave().take()`；无 replay、无 manager lock。
  7. `Drop for SemUndoAttachment` 仅 debug assertion / diagnostic，绝不可 replay 或获取 manager lock。
  8. 给 `SemManager` 增加：
     ```rust
     undo_groups: Arc<Vec<Weak<SemUndoGroup>>>,
     ```
     并在 [`SemManager::new()`，sem.rs:411-418](kernel/src/ipc/sem.rs#L411-L418) 初始化为空 slice snapshot。此任务不注册任何 group。

- [ ] **Step 4: 运行测试并确认通过，且 ABI 仍关闭。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib ipc::sem_undo::tests --target x86_64-unknown-linux-gnu
  ```

  Expected: 新 ownership tests PASS。

  Run:

  ```bash
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: `ENOSYS` gate 仍存在。

- [ ] **Step 5: 编译目标内核。**

  Run:

  ```bash
  cd .
  make kernel
  ```

  Expected: `Kernel Build Done.`，无 SEM_UNDO 相关编译或 clippy error。

- [ ] **Step 6: Commit。**

  ```bash
  git add kernel/src/ipc/mod.rs kernel/src/ipc/sem.rs kernel/src/ipc/sem_undo.rs kernel/src/process/task.rs
  git commit -m "feat(ipc): add private sem undo ownership model"
  ```

---

### Task 2: 接入 clone owner sharing，并建立最小 child rollback 安全协议

**Files:**
- Modify: `kernel/src/ipc/sem_undo.rs`
- Modify: `kernel/src/process/task.rs`
- Modify: `kernel/src/process/fork.rs:575-1100`
- Modify: `kernel/src/process/namespace/nsproxy.rs:125-200`
- Test: `kernel/src/ipc/sem_undo.rs` 的 `#[cfg(test)]`
- Test: `kernel/src/process/fork.rs` 的现有或新增 `#[cfg(test)]` 模块

**Interfaces:**
- Produces:
  ```rust
  impl ProcessControlBlock {
      pub(crate) fn prepare_shared_sem_undo_attachment(
          &self,
          ipc_ns: &Arc<IpcNamespace>,
      ) -> Result<UnpublishedSemUndoAttachmentGuard, SystemError>;
  }

  impl UnpublishedSemUndoAttachmentGuard {
      pub(crate) fn install_into(
          &mut self,
          child: &ProcessControlBlock,
      );
      pub(crate) fn disarm(self);
  }
  ```
- Consumes: Task 1 的 attachment/group/PCB API、`CloneFlags::CLONE_SYSVSEM`（[`fork.rs:37-93`](kernel/src/process/fork.rs#L37-L93)）。

- [ ] **Step 1: 先写失败测试。**

  覆盖以下最小协议，而不是用 `Arc::strong_count()` 推断 owner：

  ```rust
  #[test]
  fn ordinary_fork_child_starts_without_attachment() {
      let parent = test_pcb_with_group();
      let child = test_unpublished_child();
      assert!(child.sem_undo_group().is_none());
      assert!(parent.sem_undo_group().is_some());
  }

  #[test]
  fn sysvsem_guard_increments_once_then_install_moves_token() {
      let parent = test_pcb_with_group();
      let group = parent.sem_undo_group().unwrap();
      let child = test_unpublished_child();

      let mut guard = parent.prepare_shared_sem_undo_attachment(test_ipc_ns()).unwrap();
      assert_eq!(group.task_owners_for_test(), 2);
      guard.install_into(&child);
      assert!(child.sem_undo_group().is_some());
      guard.disarm();
      assert_eq!(group.task_owners_for_test(), 2);
  }

  #[test]
  fn installed_guard_rollback_takes_child_slot_and_only_drops_owner() {
      let parent = test_pcb_with_group();
      let group = parent.sem_undo_group().unwrap();
      let child = test_unpublished_child();

      let mut guard = parent.prepare_shared_sem_undo_attachment(test_ipc_ns()).unwrap();
      guard.install_into(&child);
      drop(guard);

      assert!(child.sem_undo_group().is_none());
      assert_eq!(group.task_owners_for_test(), 1);
      assert_eq!(group.replay_count_for_test(), 0);
  }
  ```

  并为 clone flag 前置拒绝加测试：

  ```rust
  #[test]
  fn newipc_and_sysvsem_are_rejected_before_attachment_prepare() {
      let flags = CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_SYSVSEM;
      assert_eq!(validate_semundo_clone_flags_for_test(flags), Err(SystemError::EINVAL));
  }
  ```

- [ ] **Step 2: 确认 RED。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem_undo::tests::(ordinary_fork|sysvsem_guard|installed_guard)|process::fork::tests::newipc' --target x86_64-unknown-linux-gnu
  ```

  Expected: 编译失败，因为 guard、owner increment 和 install API 尚不存在。

- [ ] **Step 3: 实现 guard 和 clone 接入。**

  1. `UnpublishedSemUndoAttachmentGuard` 持有“额外 owner token”和待安装 attachment；其 `Drop`：
     - 若已安装，调用 child `take_sem_undo_attachment()`；
     - 在 group lock 下 `task_owners -= 1`；
     - **不回放**；
     - debug-assert 减少前 owner 大于 1（parent token 必须仍存在）。
  2. `prepare_shared_sem_undo_attachment()`：
     - 使用 parent 当前 IPC ns，调用 `ensure_sem_undo_group()`；parent 没有 group 时允许创建空 group；
     - 验证 group 仍绑定该 IPC namespace；
     - 仅 group lock 下 `task_owners += 1`；
     - 以已有 group `Arc` 组成 guard；
     - 不得从 parent slot `take()`，不得把 `CLONE_THREAD` 推导为 `CLONE_SYSVSEM`。
  3. 在 [`ProcessManager::copy_process()`](kernel/src/process/fork.rs#L575-L1100) 的既有 early validation 中，在任何 child 状态/namespace 创建/owner 修改前，加入：
     ```rust
     if (clone_flags & (CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_SYSVSEM))
         == (CloneFlags::CLONE_NEWIPC | CloneFlags::CLONE_SYSVSEM)
     {
         return Err(SystemError::EINVAL);
     }
     ```
     不能依赖 [`copy_namespaces()` 中较晚的相同检查，nsproxy.rs:166-177](kernel/src/process/namespace/nsproxy.rs#L166-L177)。
  4. `copy_namespaces()` 后、PID relation / `thread_group_live` / pid links / `children` / `add_pcb()` / cgroup visibility / scheduler 可观察 child 前，若 flags 含 `CLONE_SYSVSEM`，创建 guard 并立即 `install_into(pcb)`。
  5. 目前 `copy_process()` 在 PID links 与 `children` publication 后仍可能在 [`install_reserved_fd(...)?`，fork.rs:1061-1066](kernel/src/process/fork.rs#L1061-L1066) 返回错误；所以先将 pidfd install 移进 relation publication 前的 prepare 阶段，或将 install 改为在完整 publication 前、后续不可失败的阶段完成。
  6. 最小 publication rollback 不可只复用 [`rollback_failed_fork()`，fork.rs:1103-1129](kernel/src/process/fork.rs#L1103-L1129)，因为该函数当前仅回收 pidfd/cgroup charge，未撤销 PID links、thread-group membership、children 或 global PCB。
  7. 本任务的提交目标是：将所有 guard 创建后的 fallible operation 移到 guard 创建前；guard install 后到 `copy_process()` 成功返回之间不得保留 `?` 或显式 `Err` 分支。`guard.disarm()` 在 PID/TGID/PGID/SID attach、thread group / children relation、pidfd、`ProcessManager::add_pcb()`、cgroup visible accounting 完成后执行；[`wake_up_new_task()`，manager/sched.rs:94-121](kernel/src/process/manager/sched.rs#L94-L121) 必须发生在 disarm 后。
  8. 普通 fork 不碰 child slot，天然保持 `None`。

- [ ] **Step 4: 确认 GREEN。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib ipc::sem_undo::tests --target x86_64-unknown-linux-gnu
  ```

  Expected: owner、install、rollback、flag tests PASS；无 replay 发生于 child rollback。

- [ ] **Step 5: 验证 ABI 仍保持关闭。**

  Run:

  ```bash
  cd .
  make kernel
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: kernel 成功构建，且 `ENOSYS` 仍存在。

- [ ] **Step 6: Commit。**

  ```bash
  git add kernel/src/ipc/sem_undo.rs kernel/src/process/task.rs kernel/src/process/fork.rs kernel/src/process/namespace/nsproxy.rs
  git commit -m "feat(process): track shared sem undo owners across clone"
  ```

---

### Task 3: 实现 detach/replay 基础并接入 exit，仍不开放 syscall

**Files:**
- Modify: `kernel/src/ipc/sem_undo.rs`
- Modify: `kernel/src/ipc/sem.rs:311-639, 764-780`
- Modify: `kernel/src/process/manager/exit.rs:450-488, 662-704`
- Modify: `kernel/src/process/pid.rs:649-681`（仅在现有 visibility 不够时）
- Test: `kernel/src/ipc/sem_undo.rs` 的 `#[cfg(test)]`
- Test: `kernel/src/ipc/sem.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Produces:
  ```rust
  pub(crate) fn detach_sem_undo(
      pcb: &Arc<ProcessControlBlock>,
  );

  impl SemUndoGroup {
      fn detach_owner_and_take_last_records(
          &self,
      ) -> Option<Vec<SemUndoRecord>>;
  }
  ```
- Consumes: Task 1/2 的 `take_sem_undo_attachment()`；`SemManager` checked full-ID lookup；queue rescan。

- [ ] **Step 1: 写失败的 replay 单测。**

  在 `sem_undo.rs` / `sem.rs` 的内部测试中直接用现有 [`insert_test_set()`，sem.rs:948-959](kernel/src/ipc/sem.rs#L948-L959) 风格构造 manager/set，并覆盖：

  ```rust
  #[test]
  fn last_owner_replays_adjustment_with_clamp_and_removes_record() {
      // semval = 32766, adjustment = 4 -> clamp to SEMVMX.
      // last owner replay removes record and leaves semval = 32767.
  }

  #[test]
  fn non_last_owner_does_not_replay() {
      // owner 1 detach: semval and record unchanged.
      // owner 2 detach: exactly one replay.
  }

  #[test]
  fn stale_full_semid_does_not_touch_reused_index() {
      // old set record -> RMID -> same low index, new generation.
      // replay old record; new set remains unchanged.
  }

  #[test]
  fn replay_updates_otime_and_rescans_waiter() {
      // record adjustment turns a blocked waiter Ready.
      // replay must update sem_otime, set exiting visible TGID as sempid,
      // then complete the waiter in the same manager critical section.
  }
  ```

- [ ] **Step 2: 确认 RED。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem_undo::tests::(last_owner|non_last_owner|stale_full_semid|replay_updates)' --target x86_64-unknown-linux-gnu
  ```

  Expected: 编译或 assertions 失败，因为 detach/replay 尚未实现。

- [ ] **Step 3: 实现 explicit detach 与 replay。**

  1. 新增唯一入口 `detach_sem_undo(pcb)`：
     - 首先 `pcb.take_sem_undo_attachment()`，立即释放 PCB slot lock；
     - 仅 group lock 下递减 `task_owners`；
     - 非最后 owner 直接返回；
     - 最后 owner 成为唯一 drainer，取走 records；禁止依赖 `Arc::strong_count()`、thread group、leader 或 TGID 判断。
  2. 将调用放到 [`RobustListHead::cleanup_robust_list(&pcb)` 后、`exit_mm` 前，exit.rs:450-463](kernel/src/process/manager/exit.rs#L450-L463)。
  3. 对每条 record：
     - 先仅持 group lock 取 full `SemId`；
     - upgrade group 的 namespace Weak；
     - 取得 `ipcns.sem` manager lock，再取得 group lock；
     - full-ID `get_by_semid_checked_mut(record.semid)` 成功才回放；generation mismatch / RMID 仅删除 record。
  4. 回放每个非零 adjustment：
     ```rust
     let next = (sem.val as i64 + adjustment as i64).clamp(0, SEMVMX as i64);
     sem.val = next as i32;
     sem.pid = exiting_tgid.clone();
     ```
     `exiting_tgid` 必须由 group 的 IPC namespace 对应 task 视角解析；当前 [`task_tgid_vnr()`，pid.rs:679-681](kernel/src/process/pid.rs#L679-L681) 使用 current task active pid ns，不可在跨 namespace replay 中直接使用。最小改动是将 [`task_pid_nr_ns()`，pid.rs:649-655](kernel/src/process/pid.rs#L649-L655) 提升为 `pub(crate)`，让 replay 使用 group namespace 关联的 PID namespace 获取 `PidType::TGID`。
  5. replay 发生值变动时更新 set 的 `sem_otime`，并在同一 manager 临界区复用 queue rescan。
  6. `ProcessManager::release()`（[`exit.rs:662-704`](kernel/src/process/manager/exit.rs#L662-L704)）不应首次 detach；仅加 debug assertion 确保 slot 已空。
  7. namespace Weak upgrade 失败只能按“无 live namespace state、不再回放”删除 record 并 debug-log；不可访问失效 namespace。

- [ ] **Step 4: 确认 GREEN。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem_undo::tests|ipc::sem::tests' --target x86_64-unknown-linux-gnu
  ```

  Expected: clamp、single final drainer、generation guard、`sem_otime` / queue rescan tests PASS。

- [ ] **Step 5: 编译并确认 user ABI 未发布。**

  Run:

  ```bash
  cd .
  make kernel
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: build PASS，`ENOSYS` gate 仍在。

- [ ] **Step 6: Commit。**

  ```bash
  git add kernel/src/ipc/sem_undo.rs kernel/src/ipc/sem.rs kernel/src/process/manager/exit.rs kernel/src/process/pid.rs
  git commit -m "feat(ipc): replay sem undo on final task exit"
  ```

---

### Task 4: 实现 namespace detach transaction，并保持 SEM_UNDO syscall 关闭

**Files:**
- Modify: `kernel/src/process/namespace/nsproxy.rs:294-371`
- Modify: `kernel/src/process/namespace/unshare.rs:19-53, 140-190`
- Modify: `kernel/src/process/namespace/setns.rs:96-267`
- Modify: `kernel/src/ipc/sem_undo.rs`
- Test: `kernel/src/process/namespace/nsproxy.rs` 或同目录对应 `#[cfg(test)]`
- Test: `kernel/src/ipc/sem_undo.rs` 的 owner/replay tests

**Interfaces:**
- Produces:
  ```rust
  pub(crate) struct PreparedNamespaceInstall {
      new_nsproxy: Arc<NsProxy>,
      new_fs: Option<Arc<FsStruct>>,
      new_cred: Option<Arc<Cred>>,
      detach_sysvsem: bool,
  }

  impl PreparedNamespaceInstall {
      pub(crate) fn commit(
          self,
          tsk: &Arc<ProcessControlBlock>,
          fs_refs: &FsRefsReadGuard,
      ) -> Result<(), SystemError>;
  }
  ```

**Scope boundary:** 此 task 不重写 namespace subsystem；只将已有 `setns` / `unshare` 已完成的 fallible prepare 与 publication 组合为无失败 commit，并把 SEM_UNDO detach 定义为显式 bool。

- [ ] **Step 1: 写失败测试。**

  ```rust
  #[test]
  fn unshare_sysvsem_detaches_even_when_ipc_namespace_is_unchanged() {
      // old attachment is drained before syscall returns;
      // subsequent SEM_UNDO ensure produces a different empty group.
  }

  #[test]
  fn unshare_newipc_detaches_once_when_sysvsem_is_also_present() {
      // no double decrement/replay.
  }

  #[test]
  fn setns_newipc_detaches_even_for_same_arc_target() {
      // force installation request with CLONE_NEWIPC and same Arc target;
      // old attachment is detached.
  }

  #[test]
  fn namespace_prepare_error_preserves_attachment_and_old_state() {
      // induced prepare failure: no fs/nsproxy/cred/attachment mutation.
  }
  ```

- [ ] **Step 2: 确认 RED。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'process::namespace::.*::tests|ipc::sem_undo::tests' --target x86_64-unknown-linux-gnu
  ```

  Expected: tests fail because `detach_sysvsem` is not represented or committed atomically.

- [ ] **Step 3: 实现 prepare/commit 边界。**

  1. 现状中 [`ksys_unshare()`](kernel/src/process/namespace/unshare.rs#L19-L53) 在 nsproxy/fs publication 后还可能在 `set_cred()?` 失败；[`switch_task_namespaces_inner()`](kernel/src/process/namespace/nsproxy.rs#L318-L343) 又在 `tsk.set_nsproxy()` 前修改 fs。将所有 permission、target lookup、mount path projection、new `FsStruct`、new `NsProxy`、new `Cred` 的失败工作放进 prepare。
  2. `PreparedNamespaceInstall` 显式保留 `detach_sysvsem`；不得使用 old/new IPC `Arc::ptr_eq()` 推导：
     - `unshare(CLONE_SYSVSEM)`：true；
     - `unshare(CLONE_NEWIPC)`：true；
     - 两者同时：true，且 commit 只 detach 一次；
     - pidfd 或 namespace-fd `setns`：若已验证的 installation flags 含 `CLONE_NEWIPC`，即使 target IPC Arc 与 old Arc 相同也 true；
     - 其他 UTS/NET/MNT/CGROUP/PID-for-children switch：false。
  3. commit 开始后按以下顺序执行，无 allocation、无 user-memory access、无 permission check、无新的 fallible work：
     1. 若 `detach_sysvsem`，调用 Task 3 的 synchronous `detach_sem_undo(tsk)`；
     2. 安装已准备好的 fs root/pwd 或 `FsStruct`；
     3. `tsk.set_nsproxy(new_nsproxy)`；
     4. 若有，`tsk.set_cred(new_cred)`；虽然现有 [`set_cred()`，task.rs:862-866](kernel/src/process/task.rs#L862-L866) 类型为 `Result`，实现恒为 `Ok(())`，在该 commit 中应改为或包裹为无法失败的内部 install API，避免 PONR 后错误返回。
  4. pidfd-setns 与 namespace-fd setns 在 [`ksys_setns()`](kernel/src/process/namespace/setns.rs#L96-L267) 的两条分支都改为创建同一 prepared install 后 commit。
  5. `exec_task_namespaces()`（[`nsproxy.rs:283-291`](kernel/src/process/namespace/nsproxy.rs#L283-L291)）传 `detach_sysvsem = false`，确保 exec 保留 attachment。
  6. 保持 `CLONE_NEWIPC | CLONE_SYSVSEM` 在 fork early check 拒绝；不要尝试把 unshare 的两个标志误判为非法组合。

- [ ] **Step 4: 确认 GREEN。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'process::namespace::.*::tests|ipc::sem_undo::tests' --target x86_64-unknown-linux-gnu
  ```

  Expected: four namespace detach tests PASS；prepare failure 不改变 old attachment。

- [ ] **Step 5: 编译并确认 ENOSYS 仍在。**

  Run:

  ```bash
  cd .
  make kernel
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: kernel PASS；门禁仍未移除。

- [ ] **Step 6: Commit。**

  ```bash
  git add kernel/src/process/namespace/nsproxy.rs kernel/src/process/namespace/unshare.rs kernel/src/process/namespace/setns.rs kernel/src/process/task.rs kernel/src/ipc/sem_undo.rs
  git commit -m "fix(process): detach sem undo during namespace transitions"
  ```

---

### Task 5: 重构 semop 为 lock 外 prepare + 无分配 simulation/commit

**Files:**
- Modify: `kernel/src/ipc/sem.rs:6-10, 249-309, 376-639, 663-761`
- Modify: `kernel/src/ipc/sem_undo.rs`
- Test: `kernel/src/ipc/sem.rs:930-1008`
- Test: `kernel/src/ipc/sem_undo.rs`

**Interfaces:**
- Produces:
  ```rust
  struct SemopScratch {
      // fixed-capacity, pre-reserved ordered virtual state
  }

  enum SemopOutcome {
      Ready(SemopSimulation),
      Blocked(SemBlockedOp),
  }

  impl SemManager {
      fn simulate_semop(
          set: &KernelSemSet,
          sops: &[PosixSemBuf],
          undo: Option<&mut SemUndoRecord>,
          scratch: &mut SemopScratch,
      ) -> Result<SemopOutcome, SystemError>;
  }
  ```
- Changes:
  ```rust
  struct SemQueueEntry {
      sops: Vec<PosixSemBuf>,
      pid: Option<Arc<Pid>>,
      undo_group: Option<Arc<SemUndoGroup>>,
      waker: Arc<Waker>,
      status: SpinLock<SemQueueStatus>,
  }
  ```

- [ ] **Step 1: 写 RED tests：逐项 transactional simulation。**

  先在 `sem.rs` 的测试模块中扩展已有 `insert_test_set()`，覆盖：

  ```rust
  #[test]
  fn ordered_mixed_undo_ops_apply_each_adjustment_step() {
      // Same semnum, e.g. +3|UNDO, -1 without UNDO, -2|UNDO.
      // Verify virtual semval and semadj after every element, not only net sum.
  }

  #[test]
  fn intermediate_adjustment_overflow_is_erange_even_if_later_op_cancels_it() {
      // Begin adjustment at 32767; operation sequence +1|UNDO, -1|UNDO.
      // Must return ERANGE and mutate neither semval nor semadj.
  }

  #[test]
  fn blocked_or_nowait_failure_does_not_commit_semval_or_semadj_prefix() {
      // First op is successful UNDO; second op blocks/NOWAIT.
      // Both semval and record adjustment remain original.
  }

  #[test]
  fn zero_undo_op_can_prepare_zero_record_without_adjustment() {
      // sem_op = 0 | SEM_UNDO has group/record preparation but adjustment remains 0.
  }
  ```

- [ ] **Step 2: 确认 RED。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem::tests::(ordered_mixed|intermediate_adjustment|blocked_or_nowait|zero_undo)' --target x86_64-unknown-linux-gnu
  ```

  Expected: tests fail because old simulation neither receives undo record nor validates `semadj` incrementally.

- [ ] **Step 3: 实现 scratch、record prepare 和立即路径 commit。**

  1. 删除 `hashbrown::HashMap` 对 `SemopSimulation` 的依赖；现有 [`SemopSimulation { values: HashMap<usize, i32> }`，sem.rs:376-380](kernel/src/ipc/sem.rs#L376-L380) 改为 lock 外 `try_reserve_exact(sops.len())` 的有界 ordered scratch。每个 unique `sem_num` 记录：
     ```rust
     struct SemopScratchEntry {
         semnum: usize,
         initial_val: i32,
         virtual_val: i32,
         initial_adj: i16,
         virtual_adj: i16,
     }
     ```
     使用线性查找即可，最多 `SEMOPM = 500`，不要在 manager lock 里分配 HashMap。
  2. 只要任意 op 带 `SEM_UNDO`（含 `sem_op == 0`），在 manager lock 外：
     - 读取 PCB group；没有时 `ensure_sem_undo_group(&ipcns)`；
     - group 必须绑定本 syscall IPC namespace；
     - manager lock 下先验证 full semid、读取 `nsems`，随后释放锁；
     - group lock 下检查 record；缺失时锁外以 `Vec::try_reserve_exact(nsems)` + `resize` 制造 `Box<[i16]>`；
     - 仅 group lock 下预留 records 一项容量；释放；
     - 采用 Task 6 定义的 registry snapshot replacement；
     - 重新 `manager -> group` 验证 full semid / generation / nsems 并无失败插入。
  3. simulation 按原始 `sops` 顺序：
     - 先每项验证 `sem_num`；
     - `sem_op == 0`：virtual semval 非零返回 `Blocked(Zero)`；
     - nonzero：每步 `next_val = current + sem_op`，大于 `SEMVMX` 返回 `ERANGE`，小于 0 返回 `Blocked(Increase)`；
     - 若该项有 `SEM_UNDO` 且 nonzero，计算宽类型：
       ```rust
       let next_adj = current_adj as i32 - op.sem_op as i32;
       ```
       每步要求 `-32768..=32767`，否则 `ERANGE`；
     - 任一 Blocked/Error 不写共享 semval/record。
  4. `Ready` 时唯一 commit：
     - 按 scratch 写入 semval；
     - 写最终 adjustment；
     - 写受影响 `KernelSem.pid`；
     - 更新 `KernelSemSet.sem_otime`；
     - 随后 queue rescan。
  5. 仅 Task 5 结束时，直接 ready `SEM_UNDO` 仍不向用户开放，因为 syscall gate 必须保留；调用路径可以受 `cfg(test)` / internal entry 驱动验证。

- [ ] **Step 4: 确认 GREEN。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem::tests|ipc::sem_undo::tests' --target x86_64-unknown-linux-gnu
  ```

  Expected: all immediate transaction tests PASS。

- [ ] **Step 5: 审计临界区 allocation。**

  Run:

  ```bash
  cd .
  grep -nE 'HashMap::with_capacity|\\.to_vec\\(\\)|Arc::new\\(' kernel/src/ipc/sem.rs
  ```

  Expected: 在 semtimedop manager-locked section、`simulate_semop()` 和 queue creation path 中不再出现这些分配；如还有其他合理非临界区分配，提交说明必须逐个给出其 lock context。

- [ ] **Step 6: 保持门禁并编译。**

  Run:

  ```bash
  make kernel
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: build PASS；`ENOSYS` 仍存在。

- [ ] **Step 7: Commit。**

  ```bash
  git add kernel/src/ipc/sem.rs kernel/src/ipc/sem_undo.rs
  git commit -m "feat(ipc): prepare atomic sem undo accounting"
  ```

---

### Task 6: 实现 namespace registry、queue-origin attribution 与 queued commit

**Files:**
- Modify: `kernel/src/ipc/sem.rs:249-309, 311-639, 663-761`
- Modify: `kernel/src/ipc/sem_undo.rs`
- Modify: `kernel/src/process/namespace/ipc_namespace.rs:17-64`（仅为 lifecycle invariant/debug assertion 或明确 `destroy_all()`）
- Test: `kernel/src/ipc/sem.rs` 的 `#[cfg(test)]`
- Test: `kernel/src/ipc/sem_undo.rs` 的 `#[cfg(test)]`

**Interfaces:**
- Produces:
  ```rust
  impl SemManager {
      fn prepare_undo_record_and_registry(
          &mut self,
          group: &Arc<SemUndoGroup>,
          semid: SemId,
      ) -> Result<(), SystemError>;

      fn prune_and_apply_setval_undo(
          &mut self,
          semid: SemId,
          semnum: usize,
      );

      fn prune_and_apply_setall_undo(
          &mut self,
          semid: SemId,
      );

      fn remove_undo_records_for_rmid(
          &mut self,
          semid: SemId,
      );
  }
  ```

- [ ] **Step 1: 写 RED tests。**

  ```rust
  #[test]
  fn queued_undo_commits_to_captured_group_not_waker_current_task() {
      // Queue A with SEM_UNDO; change a test-current PCB before rescan.
      // Ready retry updates A's captured group only.
  }

  #[test]
  fn queued_timeout_signal_and_rmid_never_commit_adjustment() {
      // Timeout -> EAGAIN; signal -> EINTR; RMID -> EIDRM.
      // Every result preserves record adjustment.
  }

  #[test]
  fn first_record_is_registry_visible_before_setval_cleanup() {
      // Create first record, run SETVAL cleanup, then assert its target slot == 0.
  }

  #[test]
  fn stale_weak_entries_are_compacted_without_losing_live_group() {
      // Snapshot contains stale + live + candidate.
      // Replacement preserves live/candidate exactly once.
  }

  #[test]
  fn namespace_lifecycle_invariant_has_no_live_record_at_final_drop() {
      // Test-only invariant: last namespace state cannot coexist with live owner,
      // waiter, or registered group record.
  }
  ```

- [ ] **Step 2: 确认 RED。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem::tests::(queued_|first_record|stale_weak)|ipc::sem_undo::tests::namespace_lifecycle' --target x86_64-unknown-linux-gnu
  ```

  Expected: failures because `SemQueueEntry` has no `undo_group` and registry manipulation does not exist.

- [ ] **Step 3: 实现 registry replacement protocol。**

  1. `SemManager.undo_groups` 是 `Arc<Vec<Weak<SemUndoGroup>>>` snapshot。
  2. record 第一次插入前，manager lock 内只 clone old snapshot Arc 并记录 identity；锁外：
     - upgrade each weak；
     - 去掉 stale；
     - 保留 live；
     - 若当前 group 不在 live set，append downgrade(group)；
     - 用 `Vec::try_reserve_exact` 及 `Arc::try_new` 创建 replacement slice；
  3. 回到 manager lock，只有 `Arc::ptr_eq(&self.undo_groups, &old_snapshot)` 时，按 `manager -> group` 二次检查 full semid、nsems、record presence，然后无失败 swap snapshot + insert record；snapshot 已变化时释放锁、重新 snapshot/rebuild，绝不能在不一致 snapshot 下只插 record。
  4. registry scans 始终在 manager lock 中逐个 upgrade，一个 group 一次，处理后释放 group lock；不得在 group lock 内取 manager lock。
  5. stale weak 压缩也采用 snapshot replacement；不得原地 `Vec` mutate / allocate。

- [ ] **Step 4: 实现 queue ownership 和 retry commit。**

  1. `SemQueueEntry` 新增：
     ```rust
     undo_group: Option<Arc<SemUndoGroup>>,
     ```
  2. 将 [`SemQueueEntry::new()`，sem.rs:264-276](kernel/src/ipc/sem.rs#L264-L276) 改为接收预先复制的 `Vec<PosixSemBuf>`、预先 `Arc::try_new(Waker)` 结果和 captured strong observer；它本身不调用 `to_vec()`。
  3. `semtimedop()` 在进 manager lock 前：
     - prepare queue `sops` copy；
     - `Arc::try_new(SemQueueEntry { ... })`；
     - timer/waker allocation；
     - 准备失败立即 `ENOMEM` 且无 queue/semadj 变化。
  4. [`update_queue()`，sem.rs:600-639](kernel/src/ipc/sem.rs#L600-L639) 改为 manager lock 内使用 Task 5 同一 simulation/commit：
     - entry 带 `Arc<SemUndoGroup>` observer 时，在 `manager -> captured group` 方向锁 record；
     - `Weak::upgrade()` 失败是 internal invariant violation：完成 entry 的内部错误路径，绝不能给 current/waker task 记账；
     - 只有 `Ready` 同时提交 semval+semadj，随后 remove/complete/wake；
     - `Blocked`、NOWAIT、timeout、signal、RMID 都只 remove/complete，不记 adjustment。
  5. 确认 waiter syscall 存续时持有强 group（来自 PCB attachment / semtimedop local observer）；last owner detach 加 debug assertion，不能存在仍可能对该 group commit 的 queue entry。
  6. namespace lifecycle：
     - 先证明 `IpcNamespace` 的 `Arc` 被 task nsproxy / blocked syscall 保持；
     - group 只持 `Weak`；
     - namespace switch 在发布前完成 detach/replay；
     - 若该证明无法在 test/debug assertion 中建立，才新增明确调用者的 `SemManager::destroy_all()`，在 manager lock 下 delete record、不回放、完成 waiter `EIDRM`、移除 sets，最后 free index；
     - 不在 `IpcNamespace::Drop` 取 manager lock。

- [ ] **Step 5: 确认 GREEN。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem::tests|ipc::sem_undo::tests' --target x86_64-unknown-linux-gnu
  ```

  Expected: queue owner, cancellation, registry, lifecycle tests PASS。

- [ ] **Step 6: 审计 manager lock 下无新增 allocation。**

  Run:

  ```bash
  cd .
  grep -nE 'HashMap::with_capacity|\\.to_vec\\(\\)|Arc::new\\(|try_reserve|Vec::new' kernel/src/ipc/sem.rs kernel/src/ipc/sem_undo.rs
  ```

  Expected: 所有可能分配操作都位于 manager lock 外的 prepare/replacement 阶段；检查每个命中行的调用上下文。

- [ ] **Step 7: 保持 ABI 关闭并构建。**

  Run:

  ```bash
  make kernel
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: `ENOSYS` gate remains。

- [ ] **Step 8: Commit。**

  ```bash
  git add kernel/src/ipc/sem.rs kernel/src/ipc/sem_undo.rs kernel/src/process/namespace/ipc_namespace.rs
  git commit -m "feat(ipc): commit queued sem undo to originating group"
  ```

---

### Task 7: 接入 SETVAL、SETALL、IPC_RMID 的 undo cleanup

**Files:**
- Modify: `kernel/src/ipc/sem.rs:763-780, 868-927`
- Modify: `kernel/src/ipc/sem_undo.rs`
- Test: `kernel/src/ipc/sem.rs:930-1008`

**Interfaces:**
- Produces:
  ```rust
  impl SemManager {
      fn clear_undo_for_setval(&mut self, id: SemId, semnum: usize);
      fn clear_undo_for_setall(&mut self, id: SemId);
      fn discard_undo_for_rmid(&mut self, id: SemId);
  }
  ```

- [ ] **Step 1: 写 RED tests。**

  ```rust
  #[test]
  fn setval_clears_only_target_sem_adjustment_across_all_groups() {
      // Two groups, one multi-sem record.
      // SETVAL semnum 0 clears slot 0 but preserves slot 1 in both groups.
  }

  #[test]
  fn setall_clears_entire_full_semid_record_across_all_groups() {
      // SETALL zeros every adjustment in matching records only.
  }

  #[test]
  fn setval_cleanup_precedes_value_write_and_queue_rescan() {
      // SETVAL wakes queued undo operation.
      // The newly-ready operation creates new debt that survives this cleanup.
  }

  #[test]
  fn rmid_discards_record_before_index_can_be_reused() {
      // Old full ID record is removed; reuse low index with new generation;
      // old adjustment cannot affect replacement.
  }
  ```

- [ ] **Step 2: 确认 RED。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem::tests::(setval_clears|setall_clears|setval_cleanup|rmid_discards)' --target x86_64-unknown-linux-gnu
  ```

  Expected: tests fail because existing `setval()` / `setall()` only write values, and [`ipc_rmid()`，sem.rs:764-780](kernel/src/ipc/sem.rs#L764-L780) frees index before undo cleanup.

- [ ] **Step 3: 实现 cleanup order。**

  1. `setval(id, semnum, val)` 保持已有 errno order：value range → semnum → permission（[`sem.rs:869-880`](kernel/src/ipc/sem.rs#L869-L880)）。
  2. 完整 semid/permission 验证后，在同一 manager lock 内：
     1. 扫描 registry 中 live groups；
     2. 对 matching full `SemId` record 执行 `adjustments[semnum] = 0`；
     3. 写 semval、sempid、ctime；
     4. queue rescan。
  3. `setall(token, vals)` 保持既有 [`prepare_setall()` 两阶段 token 语义，sem.rs:921-927](kernel/src/ipc/sem.rs#L921-L927)：最终 full-ID token 复核成功后，先清 matching record 的整个 slice，再写全部 sem value/pid/ctime，最后 rescan。
  4. `ipc_rmid(id)`：
     1. full-ID 与 permission 验证；
     2. manager lock 下扫描 live group，删除 matching full-ID record，不回放；
     3. `complete_all_removed()` 给每个 waiter `EIDRM`；
     4. remove key/id table、更新 total；
     5. 最后 `id_allocator.free_idx(decoded.idx)`。
  5. 所有这些 scan 都遵循 `manager -> group`；不能借助 record 的 low index 判断。

- [ ] **Step 4: 确认 GREEN。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib ipc::sem::tests --target x86_64-unknown-linux-gnu
  ```

  Expected: SETVAL/SETALL/RMID cleanup + 既有 stale setall token tests PASS。

- [ ] **Step 5: 构建并确认门禁仍存在。**

  Run:

  ```bash
  cd .
  make kernel
  grep -n -A6 'SEM_UNDO' kernel/src/ipc/sem.rs
  ```

  Expected: build PASS；`ENOSYS` 未删除。

- [ ] **Step 6: Commit。**

  ```bash
  git add kernel/src/ipc/sem.rs kernel/src/ipc/sem_undo.rs
  git commit -m "feat(ipc): clear sem undo state on semaphore controls"
  ```

---

### Task 8: 增加公开 dunitest ABI 覆盖，但暂时保留 ENOSYS

**Files:**
- Modify: `user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc:41-226, 706-714`
- Test: `user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc`
- Do not modify: `user/apps/tests/dunitest/whitelist.txt`
- Do not modify: `user/apps/tests/dunitest/Makefile`

**Interfaces:**
- Replaces:
  ```cpp
  TEST(SysVSem, SemUndoRejected)
  ```
- Adds helpers using only existing public syscalls: `semget`, `semop`, `semtimedop`, `semctl`, `fork`, `clone`, `exec`, `unshare`, `setns`, pipe/eventfd/futex-style synchronization as already available to the environment.

- [ ] **Step 1: 先将当前 ENOSYS test 改为失败的 future ABI tests。**

  删除 [`SemUndoRejected`，sysv_sem_semantics.cc:706-714](user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc#L706-L714)，增加下列 12 组（每组可包含多个 gtest case）：

  1. **`SemUndoBasicAccumulationAndSign`**
     child 对初值 0 做 `+3 | SEM_UNDO` 与 `-1 | SEM_UNDO`；parent 在 child exit 前读到 2，exit 后读回 0；初值 3 的 `-2 | SEM_UNDO` exit 后回到 3。
  2. **`SemUndoArrayOrderAndRollback`**
     同 semnum 重复、混合 flags；前项 UNDO 成功但后项 NOWAIT blocked 或 ERANGE 时，调用后和 child exit 后均不出现 prefix effect。
  3. **`SemUndoAdjustmentBounds`**
     到达 `32767` / `-32768` 成功；下一项超界 `ERANGE`；同一数组中先超界而后抵消仍必须 `ERANGE`。
  4. **`SemUndoQueueCapturedOwner`**
     A `-1|SEM_UNDO` 排队；用 `GETNCNT` 确认；B 普通 `+1` 唤醒；A 成功后退出才回放，不是 B 的 exit。
  5. **`SemUndoUncommittedPaths`**
     NOWAIT、超时、signal、wait 中 RMID 分别验证 `EAGAIN/EINTR/EIDRM` 与无 undo 回放。原有 `WaitForWaiters()` 使用轮询；新增 case 必须改为 `GETNCNT/GETZCNT` 明确握手而不是 sleep 猜测。
  6. **`SemUndoSetvalSetallClearDebt`**
     两 group、多 sem：SETVAL 只清目标 sem；SETALL 清该 set 全 slice；SETVAL 唤醒后由 queue 新提交的 debt 仍在 exit replay。
  7. **`SemUndoRmidDiscardsDebt`**
     成功 UNDO 后 RMID、owner exit，确认不会回放，且无 crash。
  8. **`SemUndoForkAndCloneOwners`**
     普通 fork 不继承；`clone(CLONE_SYSVSEM | SIGCHLD)` share；先 exit 不回放、最后 owner 仅回放一次；`CLONE_THREAD` 不含 SYSVSEM 时不共享；`CLONE_NEWIPC | CLONE_SYSVSEM` 返回 `EINVAL`。
  9. **`SemUndoExecPreservesAttachment`**
     child 做 UNDO 后 `exec` 当前 binary 的 helper mode，helper exit 后观察 replay；失败 exec 后仍保留 attachment 并在 exit 回放。
  10. **`SemUndoExitClampAndWake`**
      正向/负向 clamp；用 `GETNCNT`/`GETZCNT` 确认 waiter 入队；final-owner replay 变值触发 waiter completion。
  11. **`SemUndoUnshareSysvsem`**
      share owners 中一方 `unshare(CLONE_SYSVSEM)` 后，新 UNDO 进入新 group；剩余 owner drain 旧 group；唯一 owner unshare 在 syscall return 前 drain。
  12. **`SemUndoIpcNamespaceAndErrnos`**
      `unshare(CLONE_NEWIPC)`、namespace-fd `setns`、pidfd `setns` 全 detach；same-Arc IPC setns 也 detach；权限/invalid target 的 prepare failure 保留 old attachment；回归 `EINVAL/E2BIG/EFAULT/EFBIG/EACCES`。

  注：用户 ABI 测试不得通过循环创建数万 semaphore 强迫 index reuse，也不得尝试 `SEMMNU + 1` admission；generation reuse 与 allocation failure 留给 kernel-internal tests。

- [ ] **Step 2: 确认 dunitest 编译通过而运行仍 RED。**

  Run:

  ```bash
  cd user/apps/tests/dunitest
  make build-suites
  ```

  Expected: `bin/normal/sysv_sem_semantics_test` 编译成功。

  Run:

  ```bash
  ./bin/normal/sysv_sem_semantics_test --gtest_filter='SysVSem.SemUndo*'
  ```

  Expected: FAIL；当前 kernel 在 `SEM_UNDO` 首次操作返回 `ENOSYS`，证明 tests 不是误绿。

- [ ] **Step 3: 不修改内核门禁，只整理 ABI tests。**

  确保：
  - 使用已有 `SemSet` RAII cleanup；
  - 多进程测试使用 `ChildGuard` / `WaitChildOk`；
  - 所有 blocked rendezvous 用 `GETNCNT` / `GETZCNT`、pipe 或 eventfd，不依赖固化 sleep；
  - fork/exec helper 通过 argv/环境变量选择 helper mode，保持单一 test binary；
  - QEMU 平台不支持某一 capability 时，不得无声 skip；设计应使用现有 DragonOS 支持路径。

- [ ] **Step 4: 再确认 RED。**

  Run:

  ```bash
  ./bin/normal/sysv_sem_semantics_test --gtest_filter='SysVSem.SemUndo*'
  ```

  Expected: failing cases 的共同直接原因是 `ENOSYS`，而不是 test synchronization 超时或不确定 race。

- [ ] **Step 5: Commit。**

  ```bash
  git add user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc
  git commit -m "test(ipc): add sem undo ABI conformance cases"
  ```

---

### Task 9: 最终开放 SEM_UNDO ABI，并执行全量门禁验证

**Files:**
- Modify: `kernel/src/ipc/sem.rs:671-761`
- Modify: `user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc`（仅修正因真实 ABI 暴露而发现的确定性测试问题）
- No new APIs.
- No per-set lock.
- No namespace tunable.
- No global publication refactor beyond Task 2 的最小安全协议。

**Hard prerequisites:**
- Task 1–8 所有 internal tests 已通过。
- immediate simulation/commit、queued retry、exit replay、SETVAL/SETALL、RMID、full-ID reuse protection、fork/clone owner、exec、unshare、same-Arc setns、namespace lifecycle、prepare/rollback 都有真实实现和测试。
- 审计确认没有 `group -> manager` lock acquisition、没有 PCB slot 与 manager/group lock nesting。
- `SEM_UNDO` 仍只在唯一 gate 被拦截。

- [ ] **Step 1: 最后写 RED publication assertion。**

  在 `sysv_sem_semantics.cc` 保留至少一个明确断言：

  ```cpp
  TEST(SysVSem, SemUndoBasicAccumulationAndSign) {
      // ...
      ASSERT_EQ(0, SemOp(sem.id(), &undo_inc, 1))
          << "SEM_UNDO must be published only after all lifecycle prerequisites";
  }
  ```

  这不是新测试；它确保删除 gate 是 ABI publication 而非仅 internals 完成。

- [ ] **Step 2: 运行全部 kernel-internal gates。**

  Run:

  ```bash
  cd kernel
  cargo +nightly-2026-02-24 test --lib 'ipc::sem::tests|ipc::sem_undo::tests|process::namespace::.*::tests|process::fork::tests' --target x86_64-unknown-linux-gnu
  ```

  Expected: PASS。若项目的实际 kernel test runner 不支持 host target，改用已验证的 DragonOS CI kernel-test target；不能以 `make -C kernel test` 代替，因为它明确 `--exclude dragonos_kernel`。

- [ ] **Step 3: 删除唯一 ENOSYS gate。**

  在 [`SemManager::semtimedop()`，sem.rs:671-688](kernel/src/ipc/sem.rs#L671-L688) 删除：

  ```rust
  if sops
      .iter()
      .any(|op| (op.sem_flg as u32) & SemFlags::SEM_UNDO.bits() != 0)
  {
      return Err(SystemError::ENOSYS);
  }
  ```

  其余 input validation (`sops.is_empty()`、`SEMOPM`) 保持原位。随后让 Task 5/6 的 lock-out prepare、record prepare、queue preallocation 和 manager-locked no-fail commit 成为正常路径。

- [ ] **Step 4: 确认 GREEN：局部 ABI test。**

  先本地编译：

  ```bash
  cd user/apps/tests/dunitest
  make build-suites
  ```

  再在 DragonOS QEMU dunitest 环境执行目标 binary：

  ```bash
  /opt/tests/dunitest/bin/normal/sysv_sem_semantics_test --gtest_filter='SysVSem.SemUndo*'
  ```

  Expected: `PASSED`，所有 `SemUndo*` cases 无 skipped、无 timeout。

- [ ] **Step 5: 构建与 QEMU dunitest 回归。**

  Run:

  ```bash
  cd .
  make kernel
  make test-dunit
  ```

  Expected:
  - `make kernel` 成功；
  - dunitest runner 中 `normal/sysv_sem_semantics` PASS；
  - 既有 `SysVSem` 非-UNDO tests 无回归。

  如开发环境适合走 Nix，等价完整命令为：

  ```bash
  nix develop --command make test-dunit
  ```

- [ ] **Step 6: 静态锁/分配审计。**

  Run:

  ```bash
  cd .
  grep -RIn --include='*.rs' 'sem_undo' kernel/src/ipc kernel/src/process
  grep -nE 'HashMap::with_capacity|\\.to_vec\\(\\)|Arc::new\\(' kernel/src/ipc/sem.rs
  ```

  Expected:
  - `sem_undo` 仅通过明确语义 API 修改 PCB slot/owner；
  - manager-locked semop/queue paths 无 allocation；
  - 没有 per-set lock 字段或 lock acquisition；
  - 没有以 `Arc::strong_count()` 判定 SEM_UNDO 最后 owner。

- [ ] **Step 7: Commit final ABI publication。**

  ```bash
  git add kernel/src/ipc/sem.rs user/apps/tests/dunitest/suites/normal/sysv_sem_semantics.cc
  git commit -m "feat(ipc): enable Linux-compatible sem undo"
  ```

---

## Planned Commit Boundaries

1. `feat(ipc): add private sem undo ownership model`
2. `feat(process): track shared sem undo owners across clone`
3. `feat(ipc): replay sem undo on final task exit`
4. `fix(process): detach sem undo during namespace transitions`
5. `feat(ipc): prepare atomic sem undo accounting`
6. `feat(ipc): commit queued sem undo to originating group`
7. `feat(ipc): clear sem undo state on semaphore controls`
8. `test(ipc): add sem undo ABI conformance cases`
9. `feat(ipc): enable Linux-compatible sem undo`

`ENOSYS` 必须贯穿提交 1–8；第 9 个提交是唯一 publication point。

## Plan Self-Review

- **规格覆盖：** 覆盖 `SemUndoGroup/Attachment`、PCB slot、`CLONE_SYSVSEM`、普通 fork、child rollback、task exit、exec 保留、unshare/setns detach、immediate transaction、queued transaction、full generation semid、SETVAL、SETALL、RMID、namespace lifetime、ENOMEM failure atomicity、dunitest。
- **明确排除：** 不实现 per-set lock；不新增 namespace-local semaphore tunable；不以 `SEMUME/SEMMNU` admission；不扩大全局 namespace/fork 重构。
- **现有事实锚点：** 当前 ENOSYS gate 位于 `sem.rs:683-688`；manager lock 覆盖当前 semop `sem.rs:702-735`；当前 queue copy 在 `sem.rs:264-276`；exit hook 位于 `exit.rs:450-463`；fork publication 后 PIDFD installation 仍可能失败于 `fork.rs:1061-1066`；setns/unshare 当前确有 prepare/publication 分离缺口。
- **测试可执行性注意：** dunitest 有明确构建/运行路径：`make build-suites`、`make test-dunit`。内核 `#[cfg(test)]` 必须使用能编译 `dragonos_kernel` 本体的实际 test target；不可声称 `kernel/Makefile:test` 覆盖它，因为该命令明示排除 `dragonos_kernel`。

### Critical Files for Implementation
- `kernel/src/ipc/sem.rs`
- `kernel/src/ipc/sem_undo.rs`
- `kernel/src/process/task.rs`
- `kernel/src/process/fork.rs`
- `kernel/src/process/manager/exit.rs`
