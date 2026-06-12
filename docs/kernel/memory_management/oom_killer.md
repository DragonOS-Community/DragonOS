# OOM Killer（用户态缺页 OOM）规格

:::{note}

本文档对应 [Issue #1976](https://github.com/DragonOS-Community/DragonOS/issues/1976)。
它是在 `tmp.md` 初稿基础上的实现规格，目标是为 DragonOS 增加第一版
“用户态缺页分配失败” OOM killer。

第一版只解决一个问题：当用户态 page fault 触发内存分配失败并返回
`VM_FAULT_OOM` 时，内核不能静默返回并在同一 RIP 上无限缺页。

:::

## 1. 背景

DragonOS 当前 x86_64 用户缺页路径在收到 `VM_FAULT_OOM` 后只打印日志并返回。
这会让 CPU 回到用户态后重试同一条指令，再次访问同一地址，再次触发缺页，
最终形成 livelock。

Linux 6.6 的对应语义是：

1. `handle_mm_fault()` 返回 `VM_FAULT_OOM`；
2. 用户态 page-fault 错误路径调用 `pagefault_out_of_memory()`；
3. OOM 核心挑选并杀死 victim，或者等待已有 victim 释放内存；
4. fault 路径重试或在当前线程收到 `SIGKILL` 后终止。

DragonOS 第一版不实现 Linux 的完整 OOM 子系统，但必须实现这条闭环。

## 2. 第一版范围

第一版只包含以下内容：

- 只处理用户态合法 VMA 的缺页分配失败；
- 只接入 `arch/x86_64` 用户缺页路径；
- 新增独立 `mm::oom` 核心模块；
- 以线程组为 victim 单位，复用现有 `SIGKILL` / 退出路径；
- 使用 per-mm 驻留页统计做评分；
- 提供受控等待与重试，避免重复杀进程和忙等；
- 提供可控的测试入口与用户态回归程序。

第一版明确不做：

- swap / memcg / NUMA / cpuset OOM domain；
- `oom_score_adj`、`/proc/<pid>/oom_score*`；
- 独立 OOM reaper；
- 页分配器慢速路径统一触发 OOM；
- `panic_on_oom`、`oom_kill_allocating_task` 等 sysctl。

## 3. 设计目标

实现必须满足：

1. `VM_FAULT_OOM` 不能静默返回用户态；
2. OOM victim 选择必须全局串行化；
3. victim 以线程组为单位，且共享同一 `mm` 的线程只评分一次；
4. victim 评分依据必须是实际驻留页，而不是 VMA 总字节数；
5. `PID 0`、全局 `PID 1`、内核线程、退出中任务、无用户地址空间任务不可杀；
6. OOM 路径不能在持有 mm/VMA/page/allocator 锁时扫描、发信号或等待；
7. 内核态访问用户地址失败仍走异常表修复或 kernel fault 规则；
8. `SIGSEGV`、`SIGBUS`、COW、文件映射 fault 语义保持不变；
9. 没有可杀 victim 时必须进入明确 fatal OOM 路径。

## 4. Linux 6.6 语义基线

参考实现：

- `arch/x86/mm/fault.c:1466` 在用户态 `VM_FAULT_OOM` 时调用 `pagefault_out_of_memory()`
- `mm/oom_kill.c:201` 的 `oom_badness()`
- `mm/oom_kill.c:1103` 的 `out_of_memory()`

DragonOS 第一版对齐以下原则：

- 用户态 `VM_FAULT_OOM` 进入统一 OOM 核心；
- victim 优先选择能释放最多内存的线程组；
- 当前任务不是默认 victim，也不是永久排除对象；
- 当前线程组若被选中，必须通过正常 `SIGKILL` / 退出路径终止。

## 5. 第一版明确裁决

`tmp.md` 中的几个模糊点在这里定死：

### 5.1 评分口径

第一版只维护一个稳定统计：

```rust
resident_user_pages: AtomicUsize
```

评分公式固定为：

```text
badness = resident_user_pages
```

不在第一版区分 `anon/file/shmem/page_tables`。这些细分统计以后再加。

### 5.2 受保护任务范围

第一版只保护以下对象：

- `PID 0`
- 全局 `PID 1`
- `ProcessFlags::KTHREAD`
- 已退出、正在退出、没有 `user_vm` 的任务
- `resident_user_pages == 0` 的任务

第一版不做 namespace-aware init 保护。文档和代码都按全局 `PID 1` 实现。

### 5.3 `NoVictim` 行为

第一版 `NoVictim` 的唯一行为是：

- 打印 OOM 摘要；
- `panic!`

不引入第二种“平台定义停机流程”，避免测试 oracle 漂移。

### 5.4 当前任务被选中后的控制流

当前线程组若被选为 victim：

1. OOM 核心向该线程组投递 `SIGKILL`；
2. fault 路径不得直接返回到原用户 RIP；
3. fault 路径必须显式进入当前线程的信号处理/退出路径，让 `SIGKILL` 在返回用户态前生效。

换句话说，第一版不把“trap 返回时也许会检查 pending signal”当作隐含前提。

### 5.5 进展判定

第一版的“已有 victim 已经产生进展”只认下面两类条件：

- victim 的 `user_vm` 已经解绑或 `AddressSpace` 已析构；
- victim 的 `resident_user_pages` 已降为 0。

第一版不依赖“空闲页增长到某个阈值”作为重试条件。

## 6. 总体架构

新增 `kernel/src/mm/oom.rs`，由 `kernel/src/mm/mod.rs` 导出。

模块边界：

```text
mm::oom
├── OomContext
├── OomOutcome
├── OomCandidate
├── OomVictimState
├── pagefault_out_of_memory()
├── select_victim()
├── oom_badness()
├── commit_victim()
└── notify_mm_released()
```

其中：

- `arch/x86_64/mm/fault.rs` 只负责识别用户态 `VM_FAULT_OOM`、释放 fault 相关 guard 并调用 `mm::oom`
- `mm::oom` 负责选择、提交通知、等待、重试裁决
- 退出路径负责在 `user_vm` 脱离线程组时通知 OOM 模块

## 7. 数据结构

### 7.1 OOM 上下文

```rust
pub struct OomContext {
    pub trigger_pid: RawPid,
    pub trigger_tgid: RawPid,
    pub fault_address: VirtAddr,
    pub fault_ip: usize,
    pub order: usize,
}
```

约束：

- 第一版 `order` 固定表达本次 fault 所需页数，普通匿名缺页为 1；
- 上下文不允许持有地址空间、VMA、页表、分配器的锁或可变借用。

### 7.2 对外返回值

```rust
pub enum OomOutcome {
    Retry,
    CurrentTaskKilled,
    NoVictim,
}
```

语义：

- `Retry`：其他 victim 已提交并且当前 fault 线程已经等待到 victim 退出进展；
- `CurrentTaskKilled`：当前线程组就是 victim，fault 路径必须立刻进入信号/退出处理；
- `NoVictim`：没有可杀任务或 OOM 核心无法安全推进，fault 路径进入 fatal OOM。

### 7.3 全局 victim 状态

```rust
struct OomVictimState {
    generation: u64,
    tgid: RawPid,
    mm: Weak<AddressSpace>,
}
```

还需要一个全局等待队列，用于等待已有 victim 的进展。

## 8. RSS 统计要求

### 8.1 新增统计位置

`AddressSpace` 外层结构新增原子字段：

```rust
resident_user_pages: AtomicUsize
```

必须提供：

- `resident_pages() -> usize`
- `account_present_page_add()`
- `account_present_page_sub()`

### 8.2 计数规则

必须覆盖以下路径：

- 匿名缺页建立新 PTE；
- 文件缺页建立新 PTE；
- `fault_around` 批量建立新 PTE；
- 零页/共享匿名页建立新 PTE；
- `munmap` / `unmap_range` / `unmap_all` 解除 present PTE；
- 进程退出导致地址空间解除映射。

规则：

- 只有“从无映射到 present 映射”的成功安装才加 1；
- COW 用新页替换旧页时，总 RSS 不变；
- 解除 present 映射时减 1；
- 所有减法使用饱和减法，并在 debug 构建打印失配诊断；
- RSS 快照必须无须获取 `mm.write()`。

## 9. 候选扫描与评分

候选扫描流程：

1. 获取进程快照并立即释放全局表锁；
2. 归一化到线程组组长；
3. 按 TGID 去重；
4. 读取 `user_vm`；
5. 过滤不可杀对象；
6. 读取 `resident_user_pages`；
7. 选取 badness 最大者。

平分规则：

1. `resident_user_pages` 更大者优先；
2. 相同则 TGID 更大者优先。

这个规则必须完全确定，不允许依赖 `HashMap` 遍历顺序。

## 10. victim 提交与等待

提交顺序固定为：

```text
重新验证候选
  -> 建立 victim state
  -> 向线程组发送 SIGKILL
  -> 发布 generation
  -> 释放 OOM 锁
  -> 当前 fault 线程按 outcome 等待或退出
```

规则：

- 同一时刻只允许一个 in-flight victim；
- 若已有 victim 且尚未释放 `mm`，新触发者必须等待，不得立即挑第二个 victim；
- 等待使用 `generation + WaitQueue`；
- 等待期间不得持有 OOM 锁和 MM 相关锁；
- 第一版允许诊断超时，但超时后只重新检查同一个 victim，不能直接杀第二个。

## 11. fault 路径接入

`kernel/src/arch/x86_64/mm/fault.rs` 的 `VM_FAULT_OOM` 分支改为：

```text
if kernel access:
    exception-table fixup / kernel fault
else:
    release all mm/vma/page guards
    outcome = mm::oom::pagefault_out_of_memory(context)
    match outcome:
        Retry             => 重新走 fault loop
        CurrentTaskKilled => 显式执行当前线程的信号/退出处理
        NoVictim          => fatal OOM
```

这里的关键要求是：

- `Retry` 不是返回用户态后“让用户自己再 fault 一次”；
- `CurrentTaskKilled` 不是 `return`；
- 所有其他 fault 错误语义保持不变。

## 12. 退出路径接入

在进程退出释放 `user_vm` 的路径上，OOM 模块必须收到通知。

第一版通知条件：

- 线程组退出时，当前 PCB 即将把 `user_vm` 置为 `None`；
- 或者 `AddressSpace` 在 drop / `unmap_all` 后 RSS 归零。

通知效果：

- 若该 `mm` 对应当前 in-flight victim，则唤醒等待者；
- 清理全局 victim 状态，允许后续 generation 开始。

## 13. fatal OOM

出现下面任一情况时进入 fatal OOM：

- 候选扫描后没有可杀任务；
- OOM 核心无法安全完成 victim 提交；
- fault 路径收到 `NoVictim`。

fatal OOM 至少打印：

- trigger pid / tgid
- fault addr / ip
- 候选扫描数与过滤数
- 选中前的最大 RSS

然后 `panic!`。

## 14. 测试要求

第一版必须提供：

1. 一个可控的“用户缺页分配失败”测试入口；
2. 一个 `user/apps/c_unitest` 回归程序；
3. 至少覆盖以下场景：

- 当前任务是唯一可杀对象时，被 `SIGKILL` 终止，不再 livelock；
- 存在更大 RSS 的其他线程组时，其他线程组被杀，触发者重试成功；
- `PID 1` / kthread 不会成为 victim；
- 内核态 `copy_from_user` 等异常表回归不受影响。

## 15. 实现顺序

按下面顺序实施：

1. per-mm `resident_user_pages` 统计；
2. `mm::oom` 核心；
3. 退出通知；
4. x86_64 fault 接入；
5. 故障注入与 `c_unitest` 回归；
6. `make fmt`、`make kernel`、测试验证。

## 16. 参考

- [Issue #1976](https://github.com/DragonOS-Community/DragonOS/issues/1976)
- `/home/sparkzky/DragonOS/tmp.md`
- `/home/sparkzky/dragonos-dev/linux-6.6.21/mm/oom_kill.c`
- `/home/sparkzky/dragonos-dev/linux-6.6.21/arch/x86/mm/fault.c`
