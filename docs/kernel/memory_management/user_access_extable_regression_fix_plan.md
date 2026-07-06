# user access异常表回归根因修复计划

## 背景

PR #1997 将 `read_one_from_user()`、`copy_one_from_user()`、`copy_one_to_user()` 等单值用户访问路径改为通过异常表保护的拷贝实现，目标是避免内核直接解引用用户地址并提升 `EFAULT` 处理鲁棒性。

CI 中 `Dunitest` 报告在 `normal/sync_file_range` 后 120 秒无输出超时。本地复现同一分支时，在更早的 `normal/capability` 和 `normal/devpts_dir_read` 用例中观察到明确 panic：

```text
assertion `left == right` failed
  left: 1
 right: 0
dragonos_kernel::sched::schedule()
dragonos_kernel::libs::wait_queue::block_current_impl()
dragonos_kernel::libs::rwsem::RwSem::read()
dragonos_kernel::mm::ucontext::address_space::AddressSpace::read_guard_no_reservation_conflict()
dragonos_kernel::syscall::user_access::user_accessible_len()
dragonos_kernel::syscall::user_access::UserBufferWriter::copy_one_to_user::<StackUser>()
dragonos_kernel::ipc::syscall::sys_sigaltstack::SysAltStackHandle::handle()
```

这说明 CI 的无输出超时不是 `sync_file_range` 本身语义失败，而是前序测试已经触发内核 panic 或进入异常状态，最终表现为测试监控超时。

## 根因

### 1. 普通 uaccess 路径引入了可睡眠预检查

当前 `copy_from_user_protected()` 和 `copy_to_user_protected()` 在执行异常表拷贝前调用 `user_accessible_len()`。该函数会：

1. 获取当前进程地址空间。
2. 通过 `AddressSpace::read_guard_no_reservation_conflict()` 获取地址空间读 guard。
3. 该 guard 底层是 `RwSem`，可能进入 `WaitQueue::wait_until()`。
4. 等待路径会调用 `schedule()`。

因此，普通单值 `put_user/get_user` 风格 API 在实际拷贝前变成了可能睡眠的 API。

### 2. `sys_sigaltstack` 在 irqsave/禁抢占锁内访问用户内存

`sys_sigaltstack` 当前先获取 `pcb.sig_altstack_mut()`，该锁是 `write_irqsave()`，持有期间禁中断并增加 `preempt_count`。随后在锁内执行：

```rust
let mut writer = UserBufferWriter::new(old_ss, size_of::<StackUser>(), true)?;
writer.copy_one_to_user(&old_stack_user, 0)?;
```

当 `copy_one_to_user()` 进入 `user_accessible_len()` 并阻塞时，调度器发现当前任务 `preempt_count != 0`，触发断言。

即使把 `user_accessible_len()` 换成 `access_ok()`，如果真实写用户页时发生 lazy allocation、COW 或栈增长，page fault handler 仍可能获取地址空间写锁并睡眠。因此，用户内存访问不能发生在 `sig_altstack_mut()` 这类 irqsave/禁抢占 guard 内。

## Linux 6.6 对照

需要遵循以下分层，而不是把深度 VMA/PTE 检查混进普通 uaccess：

1. `put_user()`、`get_user()`、`copy_to_user()`、`copy_from_user()` 的普通入口只做 `access_ok()` 范围检查，然后执行带异常表的实际访问。
2. `fault_in_writeable()`、`fault_in_readable()`、GUP 等主动预触发或 pin 用户页的接口是独立 API，用于调用者明确需要提前 fault-in 或保证后续循环进展的场景。
3. Linux `sys_sigaltstack` 先 `copy_from_user()` 到内核临时变量，再调用 `do_sigaltstack()` 操作内核态结构，最后在不持 sigaltstack 锁的情况下 `copy_to_user()` 写回旧值。

该分层的要点是：普通 uaccess 可以 fault，但只能在允许缺页处理睡眠的上下文中执行；原子上下文或禁抢占上下文必须避免可能缺页的用户访问，或者使用明确的 inatomic/no-fault 语义。

## 修复目标

1. 保留单值用户访问的异常表保护，避免重新退回直接解引用用户地址。
2. 普通 `copy_{to,from}_user_protected()` 不主动调用会睡眠的 `user_accessible_len()`。
3. `sys_sigaltstack` 不在 `sig_altstack_mut()` guard 内执行任何用户内存读写。
4. 保留 `*_checked` 语义中对映射/权限的深度检查能力，但明确它是可能睡眠的 checked/prefault 风格路径，不能被普通快速路径隐式调用。
5. 不引入测试特化逻辑，不绕开异常表机制，不牺牲 Linux 兼容语义。

## 修订后的具体方案

### 1. 修正 protected copy 的职责边界

修改 `kernel/src/syscall/user_access.rs`：

- `copy_from_user_protected(dst, src)`：
  - `len == 0` 时返回 `Ok(0)`。
  - 使用 `access_ok(src, len).map_err(|_| SystemError::EFAULT)?` 做非睡眠用户地址范围检查。
  - 执行 `MMArch::copy_with_exception_table(dst_ptr, src_ptr, len)`。
  - 返回值为 0 表示成功，否则返回 `EFAULT`。

- `copy_to_user_protected(dest, src)`：
  - `len == 0` 时返回 `Ok(0)`。
  - 使用 `access_ok(dest, len).map_err(|_| SystemError::EFAULT)?` 做非睡眠用户地址范围检查。
  - 保持 CR0.WP 开启，执行异常表拷贝。
  - 只通过异常表和 page fault handler 处理非法页、COW、lazy allocation 和权限错误。

安全性依据：

- `access_ok()` 只允许用户地址范围，避免写入内核地址。
- VMA 不存在、权限不足、只读页、不可恢复缺页会由 page fault handler 走异常表 fixup，返回 `EFAULT`。
- 合法但未驻留的用户页仍可通过正常缺页处理补齐，符合 Linux 语义。

### 2. 区分普通路径和 checked 路径

普通单值 API：

- `read_one_from_user()`
- `copy_one_from_user()`
- `copy_one_to_user()`

继续复用异常表 protected copy，但不隐式执行 `user_accessible_len()`。

checked API：

- `new_checked()`
- `read_from_user_checked()`
- `copy_from_user_checked()`
- `buffer_checked()`
- `copy_to_user_checked()`

保留或显式使用 `user_accessible_len()` / 页表权限检查，但文档和注释中要明确：这些路径可能获取地址空间锁，不能在 irqsave、禁抢占、自旋锁、RCU 读侧等不可睡眠上下文中调用。

### 3. 修正 `sys_sigaltstack` 锁生命周期与基础语义

修改 `kernel/src/ipc/syscall/sys_sigaltstack.rs`：

1. 进入 `sig_altstack_mut()` 前，如果 `ss != NULL`，先用 `UserBufferReader::new(...).read_one_from_user()` 把新 stack 读入内核局部变量。
2. 在持有 `sig_altstack_mut()` 的范围内只做：
   - 根据当前 `stack` 和 `frame.stack_pointer()` 构造 `old_stack_user`。
   - 校验 `is_on_stack`、`ss_flags`、`ss_size`。
   - 更新 `stack.sp`、`stack.flags`、`stack.size`。
3. 释放 `sig_altstack_mut()` guard 后，如果 `old_ss != NULL`，再创建 `UserBufferWriter` 并执行 `copy_one_to_user()`。

这样即使 `old_ss` 所在用户页触发 lazy allocation、COW 或栈增长，也发生在可睡眠上下文，不会破坏 `preempt_count` 不变量。

同时补齐与 Linux 6.6 一致的基础 flag 语义：

- `ss_flags` 的 mode 部分允许 `0`、`SS_DISABLE`、`SS_ONSTACK`；`SS_AUTODISARM` 作为额外 flag bit 保留。
- `SS_AUTODISARM` 下 `on_sig_stack()` 应返回 false，避免把已自动解除的备用栈误判为当前仍在备用栈上。
- 禁用备用栈时保留合法的 `SS_AUTODISARM` bit，`old_ss` 在 `SS_DISABLE | SS_AUTODISARM` 场景下也要反映该状态。

这不是为当前测试特化，而是在移动锁生命周期时顺手消除同一 syscall 中已确认的 Linux 语义差异。

### 4. 修正不可睡眠上下文中的 protected uaccess 调用点

子 agent 对抗性审查确认，除 `sys_sigaltstack` 外，还有两个必须纳入本轮修复的同类风险。

#### 4.1 FUTEX_WAKE_OP

`Futex::arch_futex_atomic_op_inuser()` 当前在 `CurrentIrqArch::save_and_disable_irq()` 后：

1. 调用 `read_one_from_user()`。
2. 直接解引用用户指针写入。

这既可能在关中断上下文中触发缺页，又绕过异常表保护。修复方向对齐 Linux 6.6：

1. 增加最小的 `pagefault_disabled` 机制。仅当内核态访问用户地址且当前任务 `pagefault_disabled > 0` 时，page fault handler 不执行可睡眠的缺页处理，而是优先走异常表 fixup。
2. 将 futex 原子操作实现为带异常表修复的 no-fault 原子用户操作：
   - `SET` 使用 `xchg`。
   - `ADD` 使用 `lock xadd`。
   - `OR`、`ANDN`、`XOR` 使用 `cmpxchg` 循环。
3. `futex_wake_op()` 在持有 futex map 锁时执行 no-fault 原子操作；如果返回 `EFAULT`，释放锁后执行 sleepable fault-in，再重试。

fault-in 不需要设计完整 GUP。对当前 DragonOS 来说，在锁外用 `PageFaultHandler::handle_mm_fault()` 对目标 u32 所在页执行一次 write fault-in，覆盖读 fault、写 fault、COW 或 lazy allocation；它发生在可睡眠上下文。若 fault-in 后映射又被并发改变，下一轮 no-fault 原子操作仍会返回 `EFAULT` 并重试或向用户返回错误。

第二轮实现审查补充要求：

- `OR`、`ANDN`、`XOR` 的 `cmpxchg` 循环必须使用 early-clobber 语义的临时寄存器约束，避免临时寄存器和 `ptr` / `oparg` 输入寄存器重叠后写错地址或算错新值。
- `oparg` / `cmparg` 必须按 Linux 6.6 对 12-bit 字段做 sign-extension，比较方向为 `oldval CMP cmparg`。

#### 4.2 PTY packet mode ioctl

`pty_set_packet_mode()` 当前先拿 `tty.contorl_info_irqsave()`，随后读用户参数。`pty_get_packet_mode()` 当前把 `&tty.contorl_info_irqsave().packet` 直接传给 `copy_one_to_user()`，临时 guard 生命周期跨过用户写回。修复方式：

- `TIOCPKT`：先在锁外读取用户参数到局部变量，再持锁更新 `packet` 和 `pktstatus`。
- `TIOCGPKT`：先在锁内读取 `packet` 到局部变量并释放锁，再写回用户空间。
- `TIOCGPKT` / `TIOCSPTLCK` 的 get 路径写回显式 `i32`，避免用 Rust `bool` 只写 1 字节到 ioctl 的 `int` 用户缓冲。

### 5. 登记但不作为本轮硬阻断的边界

`switch_finish_hook()` 中 `set_child_tid` 写回用户内存位于调度切换收尾路径。子 agent 审查显示：

- DragonOS 在该写回前已经释放切换时泄露的 `arch_info` 锁。
- x86_64 切换路径在进入 hook 前已恢复当前任务的 `preempt_count`。
- Linux 6.6 同样在 `finish_task_switch()` / `preempt_enable()` 后执行 `put_user(current->set_child_tid)`。

因此本轮不移动该路径，但应补充注释或后续 debug 断言，说明该写回必须保持在可睡眠上下文。

### 6. 清理实现冗余

检查 `user_access.rs` 中单值转换 helper 是否还有死代码或职责重复。保留少量必要的 byte-slice helper：

- `checked_user_range()`
- `value_as_bytes()`
- `value_as_bytes_mut()`
- `maybe_uninit_as_bytes_mut()`

删除未被使用的旧单值直接转换函数，避免未来误用绕过异常表。

### 7. 明确 direct slice API 边界

`UserBufferReader::buffer()`、`UserBufferWriter::buffer()`、`read_from_user()`、`read_from_user_checked()` 这类直接构造用户切片的 API 仍然存在。它们不属于本轮 protected single-value 修复的主路径，但必须在计划中明确：

- 普通 syscall 用户指针访问默认使用 protected copy。
- bulk copy API 也应通过 protected copy 落到内核缓冲，不直接 `copy_from_slice()` 触碰用户页。
- direct slice API 只能用于调用者已经保证用户页可直接访问、且不会在不可睡眠上下文触发 fault 的场景。
- 已确认的 syscall 调用点 `sendto` 不再把 checked user slice 直接传入网络层，而是先拷贝到内核 `Vec<u8>`。
- 后续应单独审查 BPF map 等 raw user access 路径；这不是本轮 PR 的必要阻断项。

## 非目标

1. 不在这次修复中设计完整的 GUP/pin 用户页体系。仅为 futex no-fault 原子用户操作增加最小 `pagefault_disabled` 支撑。
2. 不改调度器断言。`schedule()` 要求 `preempt_count == 0` 是正确的不变量。
3. 不把 `sys_sigaltstack` 特判为跳过用户写回。用户传入 `old_ss` 时必须按 Linux 语义写回旧栈信息或返回 `EFAULT`。
4. 不把合法但未 present 的用户页提前判为 `EFAULT`，否则会破坏 lazy allocation、COW 和栈增长语义。

## 风险分析

### 安全性

方案不会放宽内核地址访问，因为 `access_ok()` 仍会拒绝非用户地址和溢出范围。真实页权限仍由 page fault handler 和页表权限决定，异常表只负责把内核态用户访问失败转换为错误返回。

### 卡住与死锁

普通 uaccess 仍可能因合法用户页缺页而进入 fault handler，因此调用者必须处在可睡眠上下文。`sys_sigaltstack` 和 PTY packet mode 修复后不再持 irqsave guard 做用户访问，消除了当前已确认的锁内 uaccess 根因。

checked API 仍可能睡眠，因此需要保持显式语义，不允许普通单值访问隐式调用它。

futex no-fault 原子段不允许 sleep。它通过 `pagefault_disabled` 和异常表修复把缺页转换为 `EFAULT`，再由外层释放 futex map 锁后 fault-in/retry。

### 性能

普通单值访问从 `user_accessible_len()` 降为 `access_ok()` 加异常表拷贝，减少 VM 锁获取，性能优于当前 PR。异常表正常路径没有额外分支到 fault handler，符合小对象 uaccess 的预期成本。

### 架构职责

`user_access` 负责用户地址范围检查、异常表拷贝和错误转换；VM 深度检查继续留在 checked/prefault 风格 API；具体 syscall 负责保证不在不可睡眠锁内访问用户内存。职责边界清晰，没有把 syscall 特例塞进通用 uaccess。

## 验证计划

1. 构建与静态检查：
   - `make fmt`
   - `make kernel`
   - `git diff --check`

2. 聚焦复现：
   - 运行 `DUNITEST_PATTERN=normal/capability make test-dunit`，确认不再出现 `sys_sigaltstack -> user_accessible_len -> schedule()` panic。
   - 运行 `DUNITEST_PATTERN=normal/devpts_dir_read make test-dunit`，确认同一 panic 消失。

3. 回归覆盖：
   - 运行包含 `sigaltstack` 行为的相关 dunitest；如果现有 dunitest 没有稳定覆盖 `old_ss` 写回到未驻留但合法用户页的场景，增加聚焦 dunitest。
   - 运行或新增覆盖 `TIOCPKT` / `TIOCGPKT` 的 dunitest，确认 packet mode ioctl 不在 irqsave guard 内 uaccess。
   - 运行或新增 `FUTEX_WAKE_OP` 聚焦用例，至少覆盖合法用户页、非法用户地址和普通 wake-op 成功路径。
   - 视时间运行完整 `DUNITEST_PATTERN=normal/sync_file_range make test-dunit` 或重新触发 CI Dunitest，以确认原始 CI 超时不再出现。

4. 审查点：
   - 确认 `copy_to_user_protected()` 和 `copy_from_user_protected()` 不再调用 `user_accessible_len()`。
   - 确认 `sys_sigaltstack` 的用户读写都发生在 `sig_altstack_mut()` guard 外。
   - 确认 PTY packet mode 的用户读写都发生在 `contorl_info_irqsave()` guard 外。
   - 确认 futex wake-op 的用户原子操作是 no-fault extable 操作，且 fault-in/retry 在 futex map 锁外。
   - 确认 checked 单值 API 的实现与文档语义一致。
   - 确认没有把合法 lazy/COW 用户页错误地提前拒绝。

## 实施前审查结论

本计划不是 workaround。它修复的是两个架构层面的根因：

1. 普通 uaccess 和 checked/prefault uaccess 的职责混淆。
2. syscall 或子系统在 irqsave/禁抢占锁内执行可能缺页的用户访问。
3. futex 原子用户操作缺少 no-fault 异常表语义。

方案与 Linux 6.6 的 uaccess 和 `sigaltstack` 分层一致，保留异常表保护，避免过度设计新的 inatomic uaccess 体系，改动范围集中在 `user_access` 抽象和 `sys_sigaltstack` 锁生命周期。
