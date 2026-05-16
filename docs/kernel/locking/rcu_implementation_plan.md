# DragonOS RCU 实现与分阶段落地方案

## 文档目的

这份文档用于把 DragonOS 的 RCU 设计、当前实现状态、后续 PR 拆分方案、测试策略与风险边界沉淀为一份可执行的工程计划。

这不是一份“RCU 原理科普”，而是一份面向 DragonOS 当前代码形态的实现方案说明。

## 当前状态

截至当前提交，**PR1 已经落地**，也就是：

- 已新增通用非抢占式 RCU 基础设施：`kernel/src/rcu/mod.rs`
- 已把 `ProcessControlBlock` 扩展为区分：
  - `preempt_count`
  - `rcu_read_depth`
- 已接入 quiescent state（QS）推进点：
  - `__schedule()`
  - 从内核返回用户态前
  - x86/riscv idle 扩展静默态
  - x86 CPU 下线路径
- 已实现：
  - `rcu_read_lock()` / `rcu_read_unlock()`
  - `rcu_dereference()`
  - `rcu_assign_pointer()`
  - `call_rcu()`
  - `synchronize_rcu()`
  - `rcu_barrier()`
  - `rcu_defer_drop()`
- 已实现独立的 RCU worker 内核线程 `rcu_gp`

**还没有落地**的是：

- 具体子系统的 RCU 化迁移
- 容器级 RCU 结构改造
- `SRCU`
- `Tasks RCU`
- `LOOKUP_RCU` pathwalk

也就是说，当前内核已经具备“正确的通用 RCU 骨架”，但还没有把具体对象读路径大规模切换到 RCU。

---

## 为什么 DragonOS 现在不能直接照搬 Linux Tree RCU

DragonOS 当前具备：

- `preempt_disable/enable`
- 调度入口 `__schedule()`
- idle 线程
- 中断返回用户态公共出口
- per-cpu 基础设施
- 内核线程机制

但 DragonOS **还不具备** Linux Tree RCU 所依赖的一整套成熟环境：

- 完整的上下文跟踪（context tracking）
- 完整的 RCU softirq / nocb / callback segmentation 体系
- 完整的 lockdep / PROVE_RCU / stall detector 配套
- 已经 RCU 化的任务、PID、VFS、网络核心容器

所以当前最合理的落点不是直接复制 Linux 6.6 Tree RCU，而是：

1. 先落地**通用非抢占式 RCU**
2. 用它支撑单指针发布类对象
3. 再逐步设计容器级 RCU 方案
4. 最后再考虑 `SRCU` 或更强的 RCU flavor

这也是为什么方案拆分成多个 PR，而不是一次性把所有“看起来能用 RCU 的地方”全改掉。

---

## 设计目标

### 目标

- 符合 Linux 非抢占式通用 RCU 的基本语义
- 不引入 workaround 式“伪 RCU”
- 不把对象生命周期安全建立在侥幸时序上
- 优先保证正确性，再逐步追求读路径性能收益
- 为后续 `SRCU`、容器级 RCU、VFS/网络迁移预留清晰接口

### 非目标

- 当前阶段不实现完整 Linux Tree RCU
- 当前阶段不实现 `SRCU`
- 当前阶段不实现 `Tasks RCU`
- 当前阶段不实现 `LOOKUP_RCU`
- 当前阶段不把 `HashMap/BTreeMap/Vec` 强行改成“看起来像无锁”的结构

---

## 参考基线

### Linux 6.6.21

本方案在语义上参考 Linux 6.6.21 的以下原则：

- 非抢占式 RCU 读侧可以绑定 `preempt_disable()`
- quiescent state 可以由上下文切换、返回用户态、idle 静默态推进
- `call_rcu()` 与 `synchronize_rcu()` 必须由真正的 GP 保障
- 指针发布和读侧解引用必须通过明确的内存序原语

### Asterinas

Asterinas 对 DragonOS 有两个可借鉴点：

- RCU 读侧与“禁止抢占”绑定
- 以单独监视器推进 grace period

但 Asterinas 当前做法里“在 GP 完成点直接执行 callback”的思路不适合 DragonOS 主路径，所以 DragonOS 采用了独立 worker 来执行 callback，避免把析构/回调尾延迟压进调度路径。

---

## 核心设计

## 1. RCU flavor 选择

DragonOS 当前采用：

- **通用非抢占式 RCU（non-preemptible RCU）**

基本语义：

- `rcu_read_lock()` 进入读侧临界区
- 读侧临界区禁止抢占
- 读侧不允许睡眠
- writer 通过 GP 等待所有旧读者离开

为什么不是 preemptible RCU：

- DragonOS 当前的 `preempt_count`、调度、等待队列、锁语义都更接近“禁止抢占式读侧”
- 如果现在强上 preemptible RCU，必须同步改造任务状态跟踪、blocked readers 管理与更多调度细节，风险过高

---

## 2. 读侧状态与 `preempt_count` 分离

当前实现中，`ProcessControlBlock` 同时维护：

- `preempt_count`
- `rcu_read_depth`

这样做是必须的。

不能只用 `preempt_count` 的原因：

- `preempt_count` 同时被 spinlock/rwlock/irqsave 使用
- 仅凭 `preempt_count > 0` 无法判断是否真处于 RCU 读侧
- `synchronize_rcu()`、调试断言、未来 `PROVE_RCU` 风格检查都需要明确的 RCU 嵌套层数

当前规则：

- `rcu_read_lock()`：
  - `preempt_disable()`
  - `rcu_read_depth += 1`
- `rcu_read_unlock()`：
  - `rcu_read_depth -= 1`
  - `preempt_enable()`

这保证：

- 读侧不会被调度出去
- 同时仍能区分“只是拿了自旋锁”和“确实在 RCU 读侧”

---

## 3. Grace Period 设计

DragonOS 当前的 GP 设计是：

- 全局 `gp_seq`
- 全局 `completed_gp_seq`
- 全局 `requested_gp_seq`
- 每 CPU 一个 `RcuCpuState`
- 每轮 GP 维护一个 `waiting_cpus` 集合

### GP 启动

当发生以下事件时，会请求未来 GP：

- `call_rcu()`
- `synchronize_rcu()`
- `rcu_defer_drop()`

若当前没有活动 GP，则启动新 GP：

1. `gp_seq += 1`
2. 构造等待 CPU 集合
3. 等待所有需要参与本轮 GP 的 CPU 报告 QS

### 哪些 CPU 要参与本轮 GP

当前规则：

- 仅 online CPU 参与
- 已处于 idle 扩展静默态的 CPU 不进入等待集
- 已 offline 的 CPU 不进入等待集

### GP 完成

当 `waiting_cpus` 为空时：

- 当前 GP 完成
- `completed_gp_seq = gp_seq`
- 将 `target_gp <= completed_gp_seq` 的 callback 转移到 ready 队列

---

## 4. Quiescent State（QS）推进点

当前已接入的 QS 推进点如下。

### 4.1 调度路径

位置：

- `kernel/src/sched/mod.rs::__schedule()`

语义：

- 能执行到 `__schedule()`，说明当前任务不处于 RCU 读侧
- 因为读侧绑定了 `preempt_disable()`，调度本身就意味着一个真实 QS

这是当前最稳定、最核心的 QS 来源。

### 4.2 返回用户态

位置：

- `kernel/src/exception/entry.rs`
- `arch_switch_to_user()` 的架构路径

语义：

- 从内核回到用户态前，当前 CPU 已结束本轮内核读侧活动
- 这与 Linux 把“离开内核执行上下文”视作可用于 RCU 推进的事件是一致的

### 4.3 idle 扩展静默态

位置：

- x86 idle loop
- riscv idle loop

语义：

- CPU 进入 idle 后，对 RCU 来说属于扩展静默态
- GP 启动时不必等待已处于 idle EQS 的 CPU
- 若 CPU 在活动 GP 中转入 idle，则可立即从等待集中摘掉

### 4.4 CPU offline

位置：

- 当前已接 x86 `stop_this_cpu()` 路径

语义：

- 下线 CPU 不应卡住当前 GP
- 若它仍在 `waiting_cpus` 中，必须显式摘除

### 后续可选推进点

后续如果需要强化 GP 收敛速度，可以再考虑：

- 中断退出路径上的补充 QS
- 更精细的 syscall/exception 公共入口配套状态跟踪

但当前阶段不必过早扩展。

---

## 5. callback 模型

DragonOS 当前采用：

- callback 入队与 GP 推进解耦
- callback 执行与调度主路径解耦
- callback 在独立的 RCU worker 内核线程中执行

### 已实现接口

- `call_rcu(head, func)`
- `rcu_defer_drop<T>()`
- `rcu_barrier()`

### 为什么不用“在 GP 完成时直接执行 callback”

因为 callback 在 Rust 里往往意味着：

- `drop`
- 容器析构
- 释放对象图
- 清理复杂资源

这些都可能带来明显尾延迟。

若把它们直接塞进：

- `__schedule()`
- 中断退出
- softirq 路径

就会污染调度和中断主路径，这在 DragonOS 当前阶段是不合适的。

所以当前采用的原则是：

- 主路径只推进 GP
- 真正的回调执行由 worker 消化

---

## 6. 内存序模型

当前约束非常明确：

- 发布新指针必须走 `rcu_assign_pointer()`
- 读侧读取指针必须走 `rcu_dereference()`

当前实现里：

- `rcu_dereference()` 使用 `Acquire`
- `rcu_assign_pointer()` 使用 `Release`

约束的意义是：

- writer 在发布指针前对对象内容的初始化，对 reader 可见
- reader 读到新指针后，能看到对象已初始化的状态

当前阶段不允许子系统自己直接用裸 `AtomicPtr::store/load` 伪造 RCU 语义。

---

## 7. 调试与错误保护

当前已具备或应保持的调试约束：

- `rcu_read_unlock()` 下溢直接断言
- `__schedule()` 遇到 `rcu_read_depth != 0` 给出告警/断言
- `synchronize_rcu()` 在 RCU 读侧中调用给出告警/断言
- `call_rcu()` 同一个 `RcuHead` 重复入队直接 panic

后续建议增强：

- stall detector
- `/proc` 或 debugfs 暴露：
  - `gp_seq`
  - `completed_gp_seq`
  - pending callback 数
  - ready callback 数
  - 每 CPU `in_idle_eqs`

---

## 为什么 PR 要这样拆

很多地方“看起来都能用 RCU”，但实际上可以分成两类：

### 第一类：单指针发布对象

典型特征：

- 一个字段是 `Arc<T>` 或等价单对象引用
- 写侧是“整体替换”
- 读侧主要是“读当前版本对象”

这类对象适合优先迁移。

### 第二类：容器级并发结构

典型特征：

- `HashMap`
- `BTreeMap`
- `Vec`
- `LinkedList`
- 原地增删改
- 迭代和回收交叉

这类对象不能因为“有了 RCU 基础设施”就直接无锁化。

必须先决定一种明确模型：

- snapshot copy-on-write
- intrusive RCU list/hlist
- 专用索引结构

所以 PR 必须拆开，否则很容易在“基础设施刚加上”的时候就把容器级问题一起做坏。

---

## PR 拆分方案

## PR1：RCU 核心基础设施

### 目标

把 DragonOS 做成“拥有正确通用 RCU 骨架”的内核。

### 范围

- 新增 `kernel/src/rcu/mod.rs`
- 增加 `PCB.rcu_read_depth`
- 增加 RCU API：
  - `rcu_read_lock/unlock`
  - `rcu_dereference`
  - `rcu_assign_pointer`
  - `call_rcu`
  - `synchronize_rcu`
  - `rcu_barrier`
  - `rcu_defer_drop`
- 接入 QS 推进点：
  - 调度
  
  - 返回用户态
  - idle
  - CPU offline
- 增加独立 RCU worker 内核线程

### 当前状态

**已完成。**

### 验收标准

- `make kernel` 通过
- 调度/返回用户态/idle 路径无编译和链接问题
- 能支撑后续单指针 RCU 化工作

---

## PR2：单指针对象的第一批 RCU 化

### 目标

把最适合、最容易做对的一批“单对象引用字段”迁移到 RCU。

### 推荐对象

- `nsproxy`
- `cred`
- `sighand`

这些字段当前都是高频读、低频写、整体替换型对象。

### 迁移方式

以 `RwLock<Arc<T>>` 为例，迁移方向是：

1. 写侧仍然允许通过已有锁决定“要不要更新”
2. 一旦要发布新对象：
   - 分配/构造新 `Arc<T>`
   - 使用 `rcu_assign_pointer()` 发布
3. 旧对象不立刻 `drop`
   - 改为 `rcu_defer_drop(old_obj)`
4. 读侧不再每次都走重锁
   - 改为 `rcu_read_lock()` + `rcu_dereference()`

### 为什么先做这批

- 它们不是原地修改型容器
- 没有复杂迭代器失效问题
- 生命周期边界清楚
- 能最快验证 RCU 基础设施是否真的可用

### PR2 不做什么

- 不动 `HashMap/BTreeMap/Vec`
- 不碰 PID 全局可见性表
- 不碰 mount 树
- 不碰 procfs 目录缓存树

### PR2 验收标准

- `nsproxy/cred/sighand` 读路径不再依赖重锁 clone
- 对象替换后旧版本延迟释放
- 无 use-after-free
- 现有 process/namespace/signal 测试不退化

---

## PR3：网络命名空间中的单对象引用迁移

### 目标

用真实多核读路径验证 RCU 在网络子系统中的实际收益和稳定性。

### 推荐对象

- `default_iface`
- loopback 当前引用
- 某些 netns 下的“当前默认单对象”

### 为什么 PR3 单独拆出来

网络代码的并发形态和进程/凭证不一样：

- 会经历更频繁的跨 CPU 读
- 与 NAPI/poll/事件唤醒交互更多
- 更容易暴露 memory ordering 和生命周期问题

把它独立成一个 PR，有两个好处：

1. 出问题时容易归因
2. 不会把进程对象与网络对象的回归混在一起

### PR3 不做什么

- 不动 `device_list`
- 不动 `bridge_list`
- 不动 `netlink_socket_table`

这些都是容器级结构，不属于“单指针引用迁移”的范围。

### PR3 验收标准

- 网络默认对象读取路径可在 RCU 下安全运行
- 不引入 netns 销毁时的悬空引用
- socket 基础回归不退化

---

## PR4：容器级 RCU 设计专项

### 目标

不急着写代码，先把容器级 RCU 的方案设计完整。

详细设计见：[`container_rcu_design.md`](container_rcu_design.md)。

### 必须单独设计的对象

- `ALL_PROCESS`
- `PidNamespace::pid_map`
- procfs cached children
- mount 传播树 / mount namespace 相关可见性结构
- 订阅链 / 事件链 / 某些链表容器

### 为什么不能直接改

因为这些容器的难点不是“读的时候要不要加锁”，而是：

- 节点何时可见
- 节点何时不可见
- 旧版本什么时候能回收
- 迭代时并发删除/替换怎么保证安全
- 原地 resize / rebalance 怎么办

这不是加一个 `rcu_read_lock()` 就能解决的问题。

### PR4 输出物

每类容器都必须明确采用哪一种模型：

- snapshot copy-on-write
- intrusive RCU list/hlist
- 特制数组/索引
- “保留现有锁，不做 RCU 化”

并说明：

- 为什么选它
- 生命周期怎么保证
- 写侧成本如何
- 读侧收益是否值得

### PR4 验收标准

- 每个目标容器有清晰方案
- 实现者不需要再临场拍脑袋定模型
- 形成独立设计文档，明确哪些容器可以 RCU 化、哪些必须保留现有锁

---

## PR5：首个容器级 RCU 落地

### 目标

选择一个最容易做对、收益明确的容器级对象，完成第一例真正的容器级 RCU 化。

### 推荐优先级

优先从下面两类里选：

1. procfs 某些缓存结构
2. notifier / tracepoint / 订阅链

不建议第一刀就上：

- `ALL_PROCESS`
- `pid_map`

因为这两者语义复杂、回归面大、和进程退出时序强耦合。

### PR5 验收标准

- 至少有一个容器级结构完成“非单指针”的真实 RCU 化
- 并发读写和回收语义有测试覆盖

---

## 对 `ALL_PROCESS` 和 `pid_map` 的明确态度

这是最容易误判的点，所以单独写清楚。

### 当前结论

**不要在 PR2/PR3 里直接把 `ALL_PROCESS` 或 `pid_map` 改成“RCU 查表”。**

### 原因

它们当前本质上是：

- `HashMap<RawPid, Arc<ProcessControlBlock>>`
- `HashMap<RawPid, Arc<Pid>>`

问题不在“值是 Arc 还是不是 Arc”，而在：

- 容器本身会原地修改
- 迭代器/rehash 生命周期复杂
- 删除时机与进程退出路径耦合

如果粗暴做成：

- 写侧继续改 `HashMap`
- 读侧只加 `rcu_read_lock()`

那只是伪 RCU，不能保证安全。

### 合理方向

这两类对象要么：

- 做 snapshot COW map

要么：

- 引入适合 RCU 的专用索引结构

要么：

- 明确保留现有加锁模型，不为“无锁而无锁”

---

## 测试策略

## A. 基础语义测试

需要覆盖：

- `rcu_read_lock/unlock` 嵌套
- unlock 下溢
- `synchronize_rcu()` 至少等待一个真实 GP
- `call_rcu()` callback 只执行一次
- `rcu_barrier()` 等到历史 callback 全部完成

## B. 并发压力测试

需要覆盖：

- 多 CPU reader 持续进入读侧
- writer 周期性替换对象
- 旧对象延迟释放
- callback 中释放对象
- 高频切换、idle、返回用户态共同推进 GP

## C. 回归测试

需要覆盖：

- 进程/命名空间/信号相关路径
- 网络命名空间基础路径
- 内核线程、调度和 idle 基本路径

## D. 调试观测

建议增加：

- GP 序列号观测
- callback 队列长度观测
- 每 CPU 是否处于 idle EQS 的观测
- 长时间未完成 GP 的告警

---

## 后续扩展路线

在完成 PR1~PR5 后，后续可以按需要继续扩展：

### 方向 1：引入 `SRCU`

只有当 DragonOS 出现大量“读侧允许睡眠”的真实需求时才值得做。

例如：

- 某些需要阻塞的配置读路径
- 睡眠型订阅/回调保护

### 方向 2：更复杂的 VFS RCU 化

这要求：

- dentry/inode/path/mount 结构整体配套
- 失败回退到 ref-walk 的完整逻辑

不是当前阶段应触碰的内容。

### 方向 3：更强的调试能力

例如：

- stall detector
- 类 lockdep 的 RCU 使用检查
- callback 重入/泄露检测

---

## 实施顺序建议

建议严格按如下顺序推进：

1. PR1：基础设施
2. PR2：`nsproxy/cred/sighand`
3. PR3：网络单对象引用
4. PR4：容器级设计专项
5. PR5：第一个容器级真实落地

不要把顺序改成：

1. PR1
2. 直接改 `ALL_PROCESS`
3. 顺手改 `pid_map`

这会明显提高踩坑概率。

---

## 当前仓库内与 RCU 相关的关键文件

- `kernel/src/rcu/mod.rs`
- `kernel/src/process/mod.rs`
- `kernel/src/sched/mod.rs`
- `kernel/src/exception/entry.rs`
- `kernel/src/arch/x86_64/process/mod.rs`
- `kernel/src/arch/riscv64/process/mod.rs`
- `kernel/src/arch/x86_64/process/idle.rs`
- `kernel/src/arch/riscv64/process/idle.rs`

后续做 PR2/PR3 时，重点会继续扩展到：

- `kernel/src/process/namespace/nsproxy.rs`
- `kernel/src/process/cred.rs`
- `kernel/src/ipc/sighand.rs`
- `kernel/src/process/namespace/net_namespace.rs`

---

## 一句话总结

当前已经完成的是：

- **“把 DragonOS 做成一个拥有正确通用 RCU 骨架的内核”**

接下来 PR2/PR3 的含义分别是：

- **PR2：把进程/凭证/信号这类单对象引用读路径迁到 RCU**
- **PR3：把网络命名空间中的单对象引用读路径迁到 RCU**

它们不是“继续补基础设施”，而是“开始在具体子系统上真正使用这套 RCU 语义”。
