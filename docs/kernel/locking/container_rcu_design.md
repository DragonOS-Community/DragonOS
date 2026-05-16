# DragonOS 容器级 RCU 设计文档

## 文档目的

本文是 RCU 分阶段方案中 **PR4：容器级 RCU 设计专项** 的输出物。

PR4 不实现具体容器，不改运行时代码，而是把 DragonOS 后续要迁移的容器类对象按语义分类，明确每类对象应该采用的 RCU 模型、生命周期规则、写侧同步方式、读侧收益与禁止事项。

本文的设计结论基于：

- DragonOS 当前通用非抢占式 RCU 实现：`kernel/src/rcu/mod.rs`
- DragonOS 当前进程、PID、procfs、mount、notifier、事件链等容器形态
- Linux 6.6.21 中 PID、RCU list/hlist、IDR/XArray 的语义约束
- Asterinas 中 Rust RCU pointer、COW Vec、XArray 的抽象边界

本文不是 RCU 原理说明，而是后续 PR5 及之后实现容器级 RCU 时必须遵守的工程设计。

---

## 总体结论

DragonOS 当前已经具备通用非抢占式 RCU 骨架，但这只解决一件事：

> 已经从发布点摘除的旧对象，不能在仍可能被旧读者访问时释放。

它 **不能** 让 `HashMap`、`BTreeMap`、`Vec`、`LinkedList` 的原地修改自动变成无锁安全。

因此容器级 RCU 必须满足以下原则：

- 读侧通过 `rcu_read_lock()` 保护对象生命周期。
- 写侧仍必须有独立锁串行化。
- 容器内部节点不能在读者可能遍历时原地释放、搬迁或重平衡。
- 删除必须先从 RCU 可见发布点摘除，再经过 grace period，最后释放旧对象。
- 读侧不能把裸引用带出 RCU 读侧临界区，除非先获取了 owned 引用，例如 `Arc<T>`。

后续不允许出现这类伪 RCU：

```rust
// 错误模型：写侧仍原地修改 HashMap，读侧只套 rcu_read_lock。
let _guard = rcu_read_lock();
let value = raw_hash_map.get(&key);
```

原因是 RCU 只能延迟释放对象，不能保护 `HashMap` 的桶数组 resize、元素搬迁、迭代器失效和内部元数据并发修改。

---

## 基础模型

### 1. Snapshot COW 容器

适用对象：

- 小表
- 读多写少
- 需要一致快照
- 写侧可以接受 O(n) clone

典型形式：

- `Arc<Vec<T>>`
- `Arc<BTreeMap<K, V>>`
- `Arc<HashMap<K, V>>`

写侧流程：

1. 获取 writer lock。
2. clone 当前快照。
3. 在新快照上插入、删除或替换。
4. 通过 RCU pointer 发布新快照。
5. 旧快照通过 RCU 延迟 drop。

读侧语义：

- 读者看到某个完整快照。
- 读者可能看到旧版本，但不会看到半更新状态。
- 迭代是快照一致的。

限制：

- 不适合大表高频写。
- 不适合 PID 数字索引这类 fork/exit 热路径。
- 不适合需要就地修改单个节点状态的大对象图。

### 2. RCU ID table / XArray 类索引

适用对象：

- 整数 ID 到对象的映射
- 查找频繁
- 插入/删除需要保持 O(log n) 或接近 O(1)
- 不希望每次写都 clone 整表

典型对象：

- `PidNamespace::pid_map`
- 未来文件描述符表、设备号表等可按整数索引的对象

写侧规则：

- 由一个 writer lock 串行化结构修改。
- 分配 ID 与发布对象分两步完成。
- slot 指针使用 RCU pointer 发布和清空。
- 删除 slot 后，旧对象的 slot-owned 引用延迟到 GP 后释放。

读侧规则：

- `rcu_read_lock()` 下按 ID 查 slot。
- 成功后必须获得 owned 引用，例如 `Arc<T>`，再离开读侧。
- 不允许把 slot 内裸引用带出 guard。

与 Linux 对照：

- Linux PID 使用 namespace 内 IDR 保存 `struct pid *`。
- `find_pid_ns()` 可在 `tasklist_lock` 或 RCU read-side 下调用。
- `free_pid()` 从 IDR 删除后通过 `call_rcu()` 延迟释放 `struct pid`。

DragonOS 不应照搬 C 版 IDR 指针细节，但必须保留同样的发布、删除和延迟回收语义。

### 3. Intrusive RCU list / hlist

适用对象：

- 节点天然属于对象本体。
- 删除后读者仍可能沿 next 指针走到旧节点。
- 需要弱一致遍历，不需要快照一致。

典型对象：

- Linux `struct pid::tasks[]`
- 未来高频事件链、设备链表

写侧规则：

- 通过 writer lock 串行化 list 修改。
- `add` 使用 release 发布 next 指针。
- `del` 不能立即清空读者仍可能使用的 next 指针。
- 节点释放必须延迟到 GP 后。

读侧语义：

- 允许看到并发删除前的节点。
- 允许漏掉并发插入的新节点。
- 不承诺快照一致。

DragonOS 当前不建议 PR5 第一刀实现 intrusive list。Rust 里要安全表达 intrusive 节点、pin、所有权和延迟析构，复杂度高于 C，需要专门设计。

### 4. 保留现有锁

适用对象：

- 写侧复杂且并非性能瓶颈。
- 读侧可能睡眠。
- 容器不只是索引，还维护多结构一致性。
- 当前缺少 SRCU 或完整 VFS RCU pathwalk。

保留锁不是退让，而是正确的边界。RCU 不能为了“无锁读”破坏 Linux 语义和对象生命周期。

---

## 目标容器决策

## 1. `ALL_PROCESS`

当前形态：

- `static ALL_PROCESS: SpinLock<Option<HashMap<RawPid, Arc<ProcessControlBlock>>>>`
- `ProcessManager::find()` 加锁查表并 clone `Arc<PCB>`
- `add_pcb()`、`release()`、`exchange_tid_and_raw_pids()` 原地修改表

设计结论：

**PR4 结论为保留现有锁，不做 RCU 化。**

原因：

- 它是全局 root pid 辅助索引，不是 Linux 语义里的 namespace PID 主索引。
- 它与 `exchange_tid_and_raw_pids()`、cgroup 记账、父子关系移除、退出释放路径耦合。
- 若改成 snapshot COW，全系统 fork/exit 都要 clone 全表，成本不可控。
- 若原地改 `HashMap` 并让读侧 RCU 查表，是错误模型。

后续方向：

- 保持 `ALL_PROCESS` 作为全局管理表。
- 用户可见 PID 查找应逐步收敛到 `PidNamespace::pid_map` / 未来 `RcuIdTable`。
- 若未来需要 lockless all-task iteration，应另设专用 task list，而不是复用 `HashMap`。

## 2. `PidNamespace::pid_map`

当前形态：

- `InnerPidNamespace::pid_map: HashMap<RawPid, Arc<Pid>>`
- 与 `IdAllocator`、`last_pid`、`dead`、`child_reaper` 同在 `SpinLock<InnerPidNamespace>` 下
- `find_pid_in_ns()` 加锁 get + clone
- `free_pid()` 释放各 namespace 的 pid number，并从 `pid_map` 删除

设计结论：

**后续采用专用 `RcuIdTable<Arc<Pid>>`，不采用 COW HashMap。**

原因：

- PID 查找是高频路径，fork/exit 也是热路径，整表 clone 不合适。
- Linux 6.6 的对应模型是 namespace IDR + RCU，而不是整表 COW。
- `RawPid` 是整数索引，适合做 slot/radix/xarray 类结构。

目标语义：

- `alloc_pid_in_ns()` 在 namespace writer lock 下分配 ID。
- 分配成功后，将 `Arc<Pid>` 通过 RCU slot 发布。
- `find_pid_in_ns()` 在 RCU read-side 下读取 slot，并 clone `Arc<Pid>` 后返回。
- `release_pid_in_ns()` 在 writer lock 下清空 slot，并将 slot-owned `Arc<Pid>` 延迟 drop。
- `Pid` 内部的 `numbers` 与 namespace 的循环引用问题应在 PID 生命周期设计中解决，不能靠提前 drop 或弱化 RCU 语义绕过。

接口方向：

```rust
pub struct RcuIdTable<T> {
    // 内部结构后续实现，可选择 radix/xarray/分层数组。
}

impl<T: Send + Sync + 'static> RcuIdTable<Arc<T>> {
    pub fn load(&self, id: usize) -> Option<Arc<T>>;
    pub fn store_locked(&self, id: usize, value: Arc<T>);
    pub fn remove_locked(&self, id: usize) -> Option<Arc<T>>;
}
```

约束：

- `store_locked/remove_locked` 的调用者必须持有上层 writer lock。
- `load()` 自己进入 RCU read-side，并返回 owned `Arc`。
- 不暴露裸 slot 指针。

## 3. `Pid::tasks`

当前形态：

- `tasks: [SpinLock<Vec<Weak<ProcessControlBlock>>>; PidType::PIDTYPE_MAX]`
- `pid_task()` 加锁读取第一个可升级任务
- `tasks_iter()` 持锁迭代
- `attach_pid()` push weak
- `detach_pid()` retain 删除 weak

Linux 对照：

- Linux 使用 `struct pid::tasks[PIDTYPE_MAX]` 的 RCU hlist。
- `attach_pid()` 在 `tasklist_lock` 下 `hlist_add_head_rcu()`。
- `detach_pid()` 用 `hlist_del_rcu()`。
- `pid_task()` 在 RCU 下取 hlist first。

DragonOS 设计结论：

**第一阶段采用 snapshot COW Vec，不立即做 intrusive hlist。**

原因：

- 每个 PID 对应的 task 列表通常很小。
- Rust intrusive RCU hlist 需要额外解决 pin、节点嵌入、重复入链、延迟析构和 `Weak` 清理问题。
- COW Vec 足够表达当前 PIDTYPE 的查找和遍历语义，风险更低。

目标语义：

- 每个 `PidType` 对应一个 RCU 发布的 `Arc<Vec<Weak<PCB>>>`。
- `attach_pid()`/`detach_pid()` 在 writer lock 下 clone 小 Vec 后发布。
- `pid_task()` 读快照并 upgrade 第一个存活任务。
- `tasks_iter()` 不返回持锁 iterator，而是返回 owned `Vec<Arc<PCB>>` 或 snapshot iterator。

限制：

- 旧快照中可能包含已经退出的 Weak，读侧必须 upgrade 过滤。
- unregister/detach 后，已经开始的读者仍可能看到旧 Weak，但 upgrade 失败或状态检查会过滤。
- 如果未来 PGID/SID 大组遍历出现性能问题，再单独设计 intrusive RCU hlist。

## 4. procfs `cached_children`

当前形态：

- `ProcDir<Ops>::cached_children: RwSem<BTreeMap<String, Arc<dyn IndexNode>>>`
- `list()` 先 `populate_children()`，再读锁收集 key
- `find()` 先读缓存，validate 后返回；未命中调用 `lookup_child()`

设计结论：

**适合作为 PR5 首个落地候选之一，模型为 snapshot COW map。**

原因：

- 子项数量通常较小。
- 写侧低频，主要是懒加载。
- 读侧高频，查找和 list 可以从快照获益。
- 返回值本身是 `Arc<dyn IndexNode>`，天然适合读侧获取 owned 引用。

目标语义：

- `cached_children` 替换为 `RcuCowMap<String, Arc<dyn IndexNode>>` 或等价封装。
- `populate_children_from_table()` 在写侧构造新快照并一次发布。
- `lookup_child_from_table()` miss 后构造 inode，再通过写侧锁发布新快照。
- `validate_child()` 必须保留，尤其是动态 `/proc/<pid>`、`/proc/<pid>/fd` 等目录。

禁止事项：

- 不允许把动态 PID 目录永久缓存为不可失效节点。
- 不允许在 RCU read-side 中创建 inode、申请复杂资源或执行可能睡眠的路径。
- 动态目录若创建过程可能睡眠，应在 RCU read-side 外执行，发布阶段再加写锁。

## 5. mount namespace / mount tree

当前形态：

- `MountFS::mountpoints: Mutex<BTreeMap<InodeId, Arc<MountFS>>>`
- `MountList` 内有 `mounts`、`mfs2ino`、`ino2mp` 三个 map
- mount、umount、bind mount、propagation、rewrite_paths 会同时维护多个结构

设计结论：

**当前阶段保留现有锁，不做 RCU 化。**

原因：

- mount pathwalk 与 filesystem lookup 可能睡眠，不适合当前非抢占式 RCU。
- mount propagation 需要多结构一致性，不是单个指针发布问题。
- Linux 的 mount RCU pathwalk 依赖 seqlock、mount ref、dentry/inode/path 多层协议和失败回退。
- DragonOS 当前没有完整 `LOOKUP_RCU` 和 SRCU 支撑。

后续条件：

- 先实现 VFS pathwalk 的 RCU/ref-walk 双模式。
- 明确 mount 读侧遇到 rename/umount/propagation 冲突时如何退回加锁模式。
- 引入必要的 sequence counter 或等价版本校验。

在这些条件满足前，不应把 `MountList` 或 `mountpoints` 改成 RCU 容器。

## 6. notifier / 订阅链 / 事件链

当前形态：

- `NotifierChain` 内部是按优先级排序的 `Vec<Arc<dyn NotifierBlock<...>>>`
- `AtomicNotifierChain` 用 `SpinLock`
- `BlockingNotifierChain` 用 `RwLock`

设计结论：

**`AtomicNotifierChain` 适合 COW Vec + RCU；`BlockingNotifierChain` 暂不迁移。**

原因：

- atomic notifier 的读侧调用不应睡眠，符合非抢占式 RCU。
- notifier 注册/注销低频，call_chain 高频。
- Asterinas 的 console callback / timer softirq callback 使用 RCU COW Vec，适合作为 Rust 参考模型。
- blocking notifier 允许睡眠，应该等待 SRCU 或继续使用锁。

目标语义：

- `register()` clone 当前 Vec，按 priority 插入并发布。
- `unregister()` clone 当前 Vec，删除并发布。
- `call_chain()` 在 RCU read-side 中遍历快照。
- unregister 不保证取消已经开始的 `call_chain()`；它只保证后续新读者不再看到该 block。

约束：

- `AtomicNotifierChain::call_chain()` 中的 callback 不得睡眠。
- 如果 callback 可能注册/注销同一条 chain，需要明确是否允许重入；默认不承诺重入安全。
- `nr_to_call` 对当前快照生效，不跨快照计数。

## 7. epoll / fasync / poll 事件链

当前形态：

- epoll ready list、poll epitems 使用 `SpinLock<LinkedList<Arc<EPollItem>>>`
- fasync 使用 `Mutex<Vec<Arc<FAsyncItem>>>`
- 多数路径涉及 signal、file owner、socket 状态和 wakeup

设计结论：

**默认保留现有锁，不作为 PR5 首选。**

原因：

- 事件回调路径比普通 notifier 更容易与锁顺序、唤醒、文件生命周期交叉。
- fasync 当前使用 `Mutex`，读侧可能不满足 atomic RCU callback 条件。
- epoll 已有针对 hardirq-safe 的内层 spinlock 设计，不能为 RCU 化破坏现有 Linux 对齐语义。

后续只有在确认读侧不睡眠、回调不需要持外层阻塞锁、删除语义可弱一致后，才考虑 COW snapshot 事件订阅表。

---

## 推荐 PR5 候选

PR5 应选择风险低、收益明确、语义闭合的容器。

推荐优先级：

1. `AtomicNotifierChain` 的 COW Vec + RCU。
2. procfs 静态 `cached_children` 的 COW map。
3. `Pid::tasks` 的 COW Vec。

不建议 PR5 直接做：

- `ALL_PROCESS`
- `PidNamespace::pid_map`
- mount namespace / mount tree
- epoll ready list

原因是这些对象要么语义复杂，要么需要额外基础设施，要么收益不足以覆盖风险。

---

## 未来公共接口草案

PR4 不实现接口，但后续实现应按下面边界收敛。

### `RcuCowVec<T>`

适用：

- notifier
- 小型订阅表
- 小规模 task list

必要接口：

```rust
pub struct RcuCowVec<T> {
    // RCU-published Arc<Vec<T>>
}

impl<T: Clone + Send + Sync + 'static> RcuCowVec<T> {
    pub fn snapshot(&self) -> Arc<Vec<T>>;
    pub fn update_locked<F>(&self, f: F)
    where
        F: FnOnce(&mut Vec<T>);
}
```

语义：

- `snapshot()` 返回 owned `Arc<Vec<T>>`。
- `update_locked()` 要求调用者已经持有写侧锁。
- 旧 Vec 延迟到 GP 后 drop。

### `RcuCowMap<K, V>`

适用：

- procfs cached children
- 小型只读多、写少目录表

必要接口：

```rust
pub struct RcuCowMap<K, V> {
    // RCU-published Arc<BTreeMap<K, V>> or Arc<HashMap<K, V>>
}

impl<K, V> RcuCowMap<K, V>
where
    K: Clone + Ord + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn get(&self, key: &K) -> Option<V>;
    pub fn snapshot(&self) -> Arc<BTreeMap<K, V>>;
    pub fn update_locked<F>(&self, f: F)
    where
        F: FnOnce(&mut BTreeMap<K, V>);
}
```

语义：

- `get()` 返回 clone/owned 值，不返回借用。
- `snapshot()` 用于 list 这类一致迭代。
- 动态项仍由上层 validate。

### `RcuIdTable<T>`

适用：

- PID namespace ID 表
- 未来整数索引对象表

必要接口：

```rust
pub struct RcuIdTable<T> {
    // 分层数组、radix 或 xarray 形态由实现 PR 决定。
}

impl<T: Send + Sync + 'static> RcuIdTable<Arc<T>> {
    pub fn load(&self, id: usize) -> Option<Arc<T>>;
    pub fn store_locked(&self, id: usize, value: Arc<T>);
    pub fn remove_locked(&self, id: usize) -> Option<Arc<T>>;
}
```

语义：

- `load()` 读侧自行进入 RCU，并返回 owned `Arc`。
- `store_locked/remove_locked` 要求上层 writer lock。
- 删除后的 slot-owned 引用必须延迟释放。

---

## 测试要求

每个容器落地 PR 必须至少覆盖以下测试类别。

### 基础语义

- 插入后可查找。
- 删除后新读者不可见。
- 已开始读者可安全使用旧对象。
- 旧对象在 GP 后释放。
- 重复删除、空删除、重复注册返回正确错误。

### 并发压力

- 多 CPU 高频读。
- writer 周期性插入、删除、替换。
- 读者持续迭代时 writer 删除对象。
- `rcu_barrier()` 后确认旧对象 drop 完成。

### 子系统回归

- PID：fork/exit、PID 复用、PID namespace 销毁、PGID/SID 遍历。
- procfs：`/proc` list/find、`/proc/<pid>` 退出后失效、`/proc/net` 静态项。
- notifier：register/unregister/call_chain 顺序、priority、`nr_to_call`。
- mount：若未来触碰，必须覆盖 bind mount、umount、propagation、pivot_root、mountinfo。

### 调试断言

- RCU read-side 中不得调用可能睡眠接口。
- container API 不暴露可逃逸裸引用。
- 写侧更新 API 在调试模式下尽量检查 writer lock 前置条件。

---

## 明确禁止事项

- 禁止把标准库或 `hashbrown` 的原地修改 map 直接暴露给 RCU reader。
- 禁止在 RCU read-side 内执行可能阻塞的 inode 创建、内存回收等待、用户内存拷贝或文件系统 IO。
- 禁止为了绕过循环引用而提前释放仍可能被读者看到的对象。
- 禁止让 `synchronize_rcu()` 出现在 RCU read-side 内。
- 禁止把 `Arc` clone 当成容器结构本身的并发保护。
- 禁止在没有完整 pathwalk 回退协议前实现 `LOOKUP_RCU`。

---

## 一句话结论

DragonOS 的容器级 RCU 应先从 COW 小容器和专用 ID 索引开始，保留写侧锁，读侧返回 owned 引用；`ALL_PROCESS` 和 mount 树暂不 RCU 化，`PidNamespace::pid_map` 等到专用 `RcuIdTable` 设计落地后再迁移。
