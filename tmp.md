# OOM Killer 初步设计规范

:::{note}

本文档对应 [Issue #1976](https://github.com/DragonOS-Community/DragonOS/issues/1976)，描述 DragonOS
处理**用户态缺页分配失败**的第一版 OOM（Out Of Memory）killer 设计。

本文档是实现前的初步规范。若实现阶段发现现有内存统计、进程退出或锁模型无法满足本文约束，应先更新本文档，
不得通过忽略错误、无限重试或测试专用分支绕过问题。

:::

## 1. 背景

DragonOS 的缺页处理器可以返回 `VM_FAULT_OOM`。当前 x86_64 用户缺页路径只记录错误并返回，
没有解决原缺页，也没有终止任何进程。CPU 返回用户态后，同一条指令会再次访问同一地址并再次缺页，
因此可能在同一 RIP 上无限循环。

Linux 6.6 的相关语义分为两层：

1. 页分配器在回收、压缩等手段均无法满足分配时，通过 `out_of_memory()` 运行 OOM 策略；
2. x86 用户缺页路径观察到 `VM_FAULT_OOM` 后调用 `pagefault_out_of_memory()`，而不是静默返回。

Linux 的 victim 评分以进程实际占用的 RSS、swap 和页表页为基础，并叠加 `oom_score_adj`；
选择 victim 后为其设置 OOM victim 状态、发送 `SIGKILL`，并由 OOM reaper 尽快回收地址空间。

DragonOS 当前没有完整的页回收、swap、memcg、NUMA/cpuset OOM domain、`oom_score_adj` 和 OOM reaper。
因此第一版应保留 Linux 的核心安全语义和扩展边界，但只实现 DragonOS 当前能够可靠支持的子集。

## 2. 设计目标

第一版实现必须满足以下目标：

1. 用户态合法 VMA 的缺页分配返回 `VM_FAULT_OOM` 时，不得静默返回并形成缺页活锁；
2. OOM 处理必须全局串行化，避免多个 CPU 因同一次内存压力连续杀死多个进程；
3. victim 选择以进程可释放的实际驻留内存为主要依据，而不是虚拟地址范围；
4. 以线程组为终止单位，共享同一地址空间的线程不得被重复评分；
5. PID 0、全局 init、内核线程和没有可释放用户地址空间的任务不得成为 victim；
6. victim 必须通过正常的 `SIGKILL`/进程退出路径释放资源，不得在缺页异常上下文直接销毁 PCB；
7. 内核态访问用户地址失败时，继续遵守异常表修复或 kernel fault 规则，不进入普通用户态 OOM 流程；
8. OOM 扫描、信号投递和等待期间不得持有地址空间写锁、VMA 锁、页管理器锁或页帧分配器锁；
9. 保持 `VM_FAULT_SIGSEGV`、`VM_FAULT_SIGBUS` 和 HWPOISON 等现有错误语义不变；
10. 为后续接入 `oom_score_adj`、cgroup/memcg、回收器和 OOM reaper 保留清晰接口。

## 3. 非目标

第一版不要求实现以下 Linux 功能：

- swap 及 swap entry 计分；
- NUMA、mempolicy、cpuset 限定的 OOM domain；
- memory cgroup 局部 OOM；
- `/proc/<pid>/oom_score`、`oom_score_adj` 和 `oom_adj`；
- `panic_on_oom`、`oom_kill_allocating_task` 等 sysctl；
- Linux 完整的 GFP flag、watermark、direct reclaim 和 compaction 策略；
- 独立 OOM reaper 内核线程；
- 内核堆/slab 分配失败的通用恢复；
- 在所有页帧分配失败点自动触发 OOM killer。

以上功能不得以临时、不兼容的 ABI 实现。第一版的内部结构应允许后续增量添加。

## 4. Linux 6.6 语义基线

### 4.1 调用链

Linux 6.6 的用户缺页错误处理可概括为：

```text
page fault
  -> handle_mm_fault()
  -> VM_FAULT_OOM
  -> arch page-fault error path
  -> pagefault_out_of_memory()
  -> out_of_memory()
  -> select_bad_process()
  -> oom_kill_process()
  -> mark_oom_victim() + SIGKILL
  -> victim exit / oom reaper frees memory
```

需要注意：Linux 通常已在页分配器的慢速路径中运行过 OOM killer；
`pagefault_out_of_memory()` 是缺页路径的兜底入口。DragonOS 第一版尚无对应的页分配慢速策略，
所以可先由用户缺页错误路径调用统一 OOM 核心，但接口不得绑定在 x86_64 架构模块中。

### 4.2 victim 评分

Linux 6.6 的 `oom_badness()` 基础分数为：

```text
RSS pages + swap entries + page-table pages + normalized oom_score_adj
```

其目标是优先终止能够释放最多内存的进程。第一版 DragonOS 没有 swap 和 `oom_score_adj`，因此采用：

```text
badness = anonymous_rss + file_rss + shared_rss + page_table_pages
```

若部分统计在第一版尚不可用，允许先采用：

```text
badness = resident_user_pages
```

但必须满足：

- 统计对象是实际 present 的用户映射，而不是 VMA 字节数；
- 评分按唯一地址空间计算一次；
- 评分单位统一为页；
- 评分函数单独封装，后续可增加调整值而不改变扫描流程。

### 4.3 不可杀任务

对齐 Linux 核心规则并结合 DragonOS 架构，以下任务不可选：

- idle 任务（PID 0）；
- 当前 PID namespace 的 init，以及第一版至少明确保护全局 PID 1；
- 带 `ProcessFlags::KTHREAD` 的内核线程；
- 已经退出、正在退出或已成为 zombie/dead 的任务；
- 没有用户地址空间的任务；
- 地址空间没有可释放驻留页的任务；
- 已标记为 OOM victim 且正在退出的任务。

第一版暂不提供用户态“免杀”接口，不能把普通特权进程硬编码为不可杀对象。

### 4.4 并发与已有 victim

Linux 使用全局 `oom_lock` 串行化 OOM 选择，并检测是否已有 OOM victim 正在释放内存。
DragonOS 必须提供等价约束：

- 任一时刻最多有一个线程执行全局候选扫描和 victim 提交；
- 已有 victim 正在退出时，新的触发者优先等待回收进展，而不是立即选择第二个 victim；
- 扫描与提交必须重新验证候选状态，避免候选在扫描期间退出或更换地址空间；
- 不允许持有进程全局表锁执行地址空间遍历或发送信号。

## 5. 总体架构

新增独立的 `kernel/src/mm/oom.rs`，由 `kernel/src/mm/mod.rs` 导出。建议划分为以下部分：

```text
mm::oom
├── OomContext             # 本次 OOM 的只读上下文
├── OomOutcome             # 对调用者可见的处理结果
├── OomCandidate           # 扫描阶段持有的候选快照
├── OomVictimState         # 全局 in-flight victim 状态
├── pagefault_out_of_memory()
├── select_victim()
├── oom_badness()
├── commit_victim()
└── notify_victim_reaped() # 退出路径通知
```

架构缺页处理器只负责：

1. 识别用户态 `VM_FAULT_OOM`；
2. 释放所有 fault/MM/VMA 相关锁；
3. 构造 `OomContext`；
4. 调用 `mm::oom::pagefault_out_of_memory()`；
5. 根据返回结果重试缺页、结束当前任务或进入不可恢复处理。

victim 策略不得放入 `kernel/src/arch/x86_64/mm/fault.rs`。

## 6. 核心数据结构

以下接口名称为建议名称，实现时可按 Rust 风格调整，但语义必须保持。

### 6.1 OOM 上下文

```rust
pub struct OomContext {
    pub trigger_pid: RawPid,
    pub fault_address: VirtAddr,
    pub fault_ip: usize,
    pub order: usize,
    pub reason: OomReason,
}

pub enum OomReason {
    UserPageFault,
}
```

要求：

- 第一版只接受 `UserPageFault`；
- `order` 表示连续页帧阶数或等价分配规模，普通匿名缺页应为单页；
- 上下文不得持有 VMA、地址空间写 guard、页表 mapper 或 allocator guard；
- 后续页分配慢速路径可增加新的 `OomReason`，不改变选择器主体。

### 6.2 处理结果

```rust
pub enum OomOutcome {
    Retry,
    CurrentTaskKilled,
    NoVictim,
}
```

语义：

- `Retry`：已有或新选 victim 已经释放/开始释放内存，调用者可以重新执行一次缺页；
- `CurrentTaskKilled`：当前线程组就是 victim，调用者不得正常返回同一用户指令；
- `NoVictim`：不存在可杀任务或无法提交 victim。调用者必须走明确的 fatal OOM 路径，禁止静默返回。

内部可以有更细的状态，如 `VictimSelected`、`VictimInFlight` 和 `VictimReaped`，但架构层只依赖上述稳定语义。

### 6.3 victim 状态

全局状态至少记录：

```rust
struct OomVictimState {
    generation: u64,
    tgid: RawPid,
    mm: Weak<AddressSpace>,
    rss_pages_at_selection: usize,
    free_pages_at_selection: usize,
}
```

要求：

- 状态以线程组和地址空间为单位；
- 使用 `Weak<AddressSpace>`，避免 OOM 状态自身阻止 victim 地址空间析构；
- `generation` 用于区分连续两次 OOM 事件和避免丢失唤醒；
- 只有成功提交 `SIGKILL` 后才能发布 in-flight victim；
- victim 地址空间已析构、RSS 降为零或退出路径明确完成内存释放后，应清除该状态并唤醒等待者。

## 7. 驻留内存统计

### 7.1 统计要求

`InnerAddressSpace::vma_usage_bytes()` 只能表示虚拟地址空间总量，不能用作 OOM 主评分。
需要为 `AddressSpace` 增加可并发读取的驻留页统计，最低要求如下：

```rust
pub struct MmRssStat {
    anon: AtomicUsize,
    file: AtomicUsize,
    shmem: AtomicUsize,
    page_tables: AtomicUsize, // 若第一版无法可靠实现，可暂时为 0
}
```

并提供一致的只读快照：

```rust
pub struct MmRssSnapshot {
    pub anon: usize,
    pub file: usize,
    pub shmem: usize,
    pub page_tables: usize,
}
```

### 7.2 计数规则

- 新建 present 用户 PTE 时增加对应 mm 的 RSS；
- 同一 PTE 的权限变更不得改变 RSS；
- COW 将旧物理页替换为新物理页时，该 mm 的总 RSS 通常不变；
- `munmap`、地址空间销毁和回滚已建立映射时减少 RSS；
- 并发缺页只能由最终成功安装 PTE 的一方计数；
- 共享物理页可以在每个映射它的 mm 中各计一页，与 Linux per-mm RSS 语义一致；
- 所有减法必须防止下溢，并在 debug 构建中对失配发出诊断；
- RSS 快照不得要求获取地址空间写锁，否则 OOM 扫描容易与缺页/退出路径死锁。

### 7.3 第一阶段允许的简化

若匿名、文件和 shmem 分类需要大规模重构，可先只维护 `resident_user_pages` 总数。
该简化必须在实现 PR 中明确记录，并保证计数覆盖所有 present PTE 的建立与解除路径。
禁止退化为 VMA 大小、`brk` 大小或申请过的虚拟页数。

## 8. 候选扫描与评分

### 8.1 扫描步骤

1. 从 `ProcessManager` 获取 PID 或 PCB 的短生命周期快照，立即释放全局进程表锁；
2. 将每个任务归一化到线程组组长；
3. 按线程组 ID 去重；
4. 获取该线程组中仍持有用户地址空间的任务；
5. 运行不可杀过滤；
6. 读取 RSS 快照并计算 badness；
7. 选出分数最高的候选；
8. 在提交前重新验证候选仍存活、未退出且地址空间未更换；
9. 发布 victim 状态并投递 `SIGKILL`。

### 8.2 平分规则

候选分数相同时，第一版使用确定性规则：

1. RSS 更大者优先；
2. RSS 相同则选择 TGID 更大的进程。

确定性规则便于测试，且不得依赖 `HashMap` 遍历顺序。

### 8.3 当前任务策略

第一版不实现 Linux 的 `oom_kill_allocating_task` 开关。当前触发者与其他进程使用相同评分规则：

- 若当前线程组分数最高，可以选择当前线程组；
- 不得无条件杀死触发缺页的任务，因为分配失败不代表它占用内存最多；
- 不得无条件排除当前任务，否则只有当前任务可杀时会退化为 fatal OOM。

## 9. victim 提交与终止

### 9.1 提交顺序

建议采用以下顺序：

```text
重新验证候选
  -> 建立内部 victim reservation
  -> 向线程组投递 SIGKILL
  -> 信号提交成功后发布 in-flight 状态
  -> 释放 OOM 全局选择锁
  -> 当前触发者等待或退出
```

若信号投递失败：

- 撤销 reservation；
- 若失败原因是候选已经退出，允许重新扫描一次；
- 其他错误必须记录诊断并返回 `NoVictim`，不得伪装为成功。

### 9.2 线程组语义

OOM 终止对象是完整线程组，而不是单个线程。应复用 DragonOS 的线程组信号和退出机制：

- 通过 `PidType::TGID` 或等价接口发送 `SIGKILL`；
- 保证不可屏蔽信号唤醒睡眠线程；
- 由正常信号处理/退出路径执行文件、futex、父子关系和地址空间清理；
- 共享同一 mm 的线程不得留下仍可运行的成员。

### 9.3 PID 1 与命名空间 init

全局 PID 1 必须不可杀。若实现时已有可靠的 PID namespace init 判定，应保护触发 OOM 所在 domain 的 init；
否则第一版至少保护全局 PID 1，并在后续工作中补齐 namespace-aware 规则。

## 10. 等待、重试与进展判定

仅发送 `SIGKILL` 后立即返回用户态是不正确的：victim 可能尚未释放页面，原缺页会再次失败。
第一版必须具有受控等待和重试机制。

### 10.1 进展条件

满足以下任一条件可认为 victim 已产生进展：

- victim 的 `AddressSpace` 已析构；
- victim RSS 已下降到零；
- 页帧空闲数相对选择时增加，并达到本次分配所需规模；
- 退出路径显式通知用户地址空间已经解除绑定并开始最终回收。

### 10.2 等待规则

- 等待期间不得持有 OOM 选择锁和任何 mm/VMA/page/allocator 锁；
- 使用 generation + wait queue/事件机制，避免先通知后睡眠造成丢失唤醒；
- 可设置诊断超时，但超时不得直接选择第二个 victim；
- 超时后应重新检查 victim 和内存进展，只有确认旧 victim 无法释放内存时才允许新一轮选择；
- 不得无限忙等或在中断关闭状态下阻塞。

### 10.3 缺页重试

架构层收到 `Retry` 后，应重新进入 VMA 查找和 `handle_mm_fault()`，而不是直接返回用户态。
每次重试必须重新获取地址空间和 VMA，因为等待期间其他线程可能执行 `munmap`、`mprotect` 或退出。

若重试后变为 `SIGSEGV`/`SIGBUS`，按新的 fault 结果处理。若再次 OOM，可进入下一轮 OOM 流程，
但必须遵守已有 victim 和 generation 规则。

## 11. 锁与上下文约束

### 11.1 禁止持锁进入 OOM 核心

调用 `pagefault_out_of_memory()` 前必须释放：

- `AddressSpace` 读写 guard；
- VMA guard；
- 页表 mapper 的可变借用；
- page manager 锁；
- frame allocator 锁；
- 任何可能被进程退出或 `unmap_all()` 获取的锁。

### 11.2 推荐锁顺序

OOM 模块内部建议遵循：

```text
短暂获取 OOM 状态锁
  -> 释放
  -> 获取进程快照
  -> 逐个无锁读取原子 RSS / 短暂读取 PCB 状态
  -> 短暂获取 OOM 状态锁提交 victim
  -> 释放
  -> 发送信号或等待
```

若信号 API 要求在持有 OOM 状态锁时调用，应先调整接口；不能让信号路径反向进入 OOM/MM 锁。

### 11.3 分配约束

OOM 核心运行时系统已处于低内存状态，因此关键路径应尽量避免动态分配：

- 候选扫描优先复用已有进程快照能力；
- 日志格式化不得构造大型临时字符串；
- 不得在 OOM 路径克隆地址空间或分配与进程数线性增长的大对象；
- 若现有 `Vec` 快照不可避免，第一版可使用，但后续应改为受控、可失败的快照或迭代 API；
- OOM 路径自身的分配失败必须返回 `NoVictim`，不能递归触发 OOM。

## 12. 架构缺页路径改造

`kernel/src/arch/x86_64/mm/fault.rs` 的目标流程应为：

```text
handle_mm_fault()
  -> COMPLETED: return
  -> RETRY: 按 fault retry 规则重试
  -> OOM:
       if kernel access:
           exception-table fixup or kernel fault
       else:
           drop all mm/vma/page locks
           outcome = pagefault_out_of_memory(context)
           Retry             -> 从 VMA 查找开始重试
           CurrentTaskKilled -> 进入信号检查/调度，禁止返回原指令
           NoVictim          -> fatal OOM diagnostic
  -> SIGBUS/SIGSEGV: 保持现有信号语义
```

当前线程被选中后，可以依靠返回用户态前的 pending signal 检查终止，但实现必须证明：

- `SIGKILL` 不可被屏蔽；
- 异常返回路径一定检查 pending signal；
- 不会先重新执行 faulting RIP；
- 若该保证不成立，应在安全点显式进入信号处理或调度。

## 13. fatal OOM 行为

若系统中不存在可杀且能释放用户内存的任务，或者 OOM 核心自身无法安全运行，应进入明确的 fatal OOM 路径：

1. 输出一次完整 OOM 摘要；
2. 输出触发者、fault 地址/IP、请求页数和全局页帧统计；
3. 输出候选过滤统计，例如扫描数量及各类排除数量；
4. panic 或进入平台定义的不可恢复停机流程。

第一版不得在 `NoVictim` 时返回原用户指令，也不得将物理内存耗尽伪装为 `SIGSEGV`。

## 14. 日志与可观测性

一次成功 OOM 事件至少输出一条摘要：

```text
oom-kill: reason=user-page-fault trigger_pid=<pid> addr=<addr> ip=<ip>
          victim_tgid=<tgid> victim_name=<name> score=<pages>
          rss=<pages> free_before=<pages> total=<pages>
```

等待结束后输出结果：

```text
oom-recovery: generation=<n> freed=<pages> retrying fault
```

要求：

- 每个 generation 只输出一次完整候选表，避免低内存下日志风暴；
- 不在每次 fault retry 中重复打印相同错误；
- debug 构建可输出被过滤候选及原因；
- 日志字段保持稳定，便于自动化测试匹配。

## 15. 测试规范

### 15.1 确定性故障注入

必须提供只在测试/调试配置启用的用户缺页页帧分配故障注入点。注入点应位于用户 fault 的上层分配包装，
不能修改 buddy allocator 的正常策略。至少支持：

- 按 PID/TGID 限定触发者；
- 在 N 次成功分配后失败；
- 失败一次或持续失败；
- 默认关闭；
- 不影响内核页表、日志和进程退出所需的保底内存。

故障注入只用于稳定测试，不得参与生产 OOM 策略。

### 15.2 必测场景

1. **基本 victim 选择**：两个可杀进程中，实际 RSS 更大的线程组被终止；
2. **当前任务 victim**：只有触发者可杀时，触发者以 `SIGKILL` 退出而不是 livelock；
3. **其他任务 victim**：杀死其他任务释放内存后，触发者缺页重试成功；
4. **线程组**：多线程 victim 的所有线程均退出，地址空间只评分一次；
5. **保护任务**：PID 0、PID 1 和 KTHREAD 从不被选择；
6. **并发 OOM**：多个 CPU 同时触发时只提交一个 generation/victim；
7. **退出竞态**：候选在扫描期间自行退出时重新选择且不使用悬空引用；
8. **无 victim**：进入明确 fatal OOM，不返回同一用户 RIP；
9. **普通 fault 回归**：SIGSEGV、SIGBUS、COW 和文件映射缺页行为不变；
10. **异常表回归**：内核 `copy_from_user` 等访问失败继续返回 `EFAULT`，不触发用户 OOM killer；
11. **RSS 统计**：map/fault/COW/munmap/exit 后计数与实际 present PTE 一致；
12. **重复事件**：第一个 victim 回收完成后，后续独立 OOM 能生成新的 generation。

### 15.3 验收标准

- OOM 测试期间内核不 panic，除非测试明确覆盖 `NoVictim` fatal 路径；
- faulting task 不在同一 RIP 上无限触发 `VM_FAULT_OOM`；
- `waitpid` 观察到 victim 因 `SIGKILL` 退出；
- victim 退出后空闲页数或可分配能力确实恢复；
- `make fmt` 和 `make kernel` 通过；
- 新增内核单元测试或 `user/apps/c_unitest` 回归测试通过。

## 16. 分阶段实现计划

### Phase 1：RSS 基础设施

- 增加 per-mm 驻留页统计及快照接口；
- 覆盖匿名页、COW、文件页、munmap 和地址空间销毁路径；
- 增加统计一致性测试。

### Phase 2：OOM 核心

- 新增 `mm::oom`；
- 实现全局串行化、候选过滤、确定性评分和 victim reservation；
- 实现线程组 `SIGKILL` 提交及稳定日志；
- 暂不接入架构 fault。

### Phase 3：退出通知与等待

- 在用户地址空间释放路径通知 OOM generation；
- 实现等待、进展检测和已有 victim 复用；
- 验证不持有 MM 相关锁等待。

### Phase 4：用户缺页接入

- 重构 x86_64 用户 fault 的锁生命周期；
- 接入 `pagefault_out_of_memory()`；
- 实现 `Retry`、`CurrentTaskKilled` 和 `NoVictim` 分支；
- 保持其他 fault 信号语义。

### Phase 5：故障注入与回归测试

- 增加确定性 fault 分配失败注入；
- 覆盖单线程、多线程、并发、竞态和无 victim 场景；
- 对比 Linux 6.6 可观察行为。

## 17. 后续扩展

第一版稳定后，建议按以下顺序扩展：

1. `oom_score_adj` 内核字段与 `/proc` ABI；
2. 页表页、shmem 等更精确的评分；
3. 页分配器慢速路径统一触发 OOM；
4. OOM victim 内存保留和退出保障；
5. 独立 OOM reaper；
6. cgroup/memcg 局部 OOM；
7. NUMA/cpuset/mempolicy OOM domain；
8. sysctl 策略与 fatal OOM 配置。

## 18. 参考实现

- [DragonOS Issue #1976](https://github.com/DragonOS-Community/DragonOS/issues/1976)
- [Linux 6.6 `mm/oom_kill.c`](https://github.com/torvalds/linux/blob/v6.6/mm/oom_kill.c)
- [Linux 6.6 `arch/x86/mm/fault.c`](https://github.com/torvalds/linux/blob/v6.6/arch/x86/mm/fault.c)
- [Linux 6.6 `mm/page_alloc.c`](https://github.com/torvalds/linux/blob/v6.6/mm/page_alloc.c)
- [Linux 6.6 `include/linux/oom.h`](https://github.com/torvalds/linux/blob/v6.6/include/linux/oom.h)

