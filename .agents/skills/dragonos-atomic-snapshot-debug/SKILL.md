---
name: dragonos-atomic-snapshot-debug
description: 使用低扰动原子快照、GDB 现场采样和语义对比来调试 DragonOS 内核中的时序问题、Heisenbug、阻塞挂起、丢唤醒和“加日志现象改变”的问题。适用于网络、VFS、调度、IPC、驱动等子系统；当用户提到任务卡住、CPU idle 但请求不返回、阻塞点偶发失效，或明确要求在线取证且不想依赖高频日志时使用。
---

# DragonOS Atomic Snapshot Debug

## 目标

在不显著扰动时序的前提下，定位 DragonOS 内核中的阻塞挂起、时序敏感 bug 和事件交接异常。

核心原则：

1. 先抓挂住现场，再改代码。
2. 优先使用静态原子快照，而不是高频 `debug!`/`printk`。
3. 用现场证据逐层缩小范围：线程状态 -> wait 点 -> 子系统计数 -> 语义契约对比。

## 适用范围

优先用于这些情况：

- `read()`/`write()`、`send()`/`recv()`、`poll()`/`epoll()`、`futex`、`completion`、`wait_queue` 等阻塞路径偶发不返回
- “加日志就能过，不加日志就卡”
- 调度/中断/驱动/网络/VFS/IPC 等子系统之间疑似有事件交接丢失
- CPU 看起来 idle，但测试线程没有退出

## 工作流

### 1. 先确认这是时序问题，而不是立即可见的 panic

先收集最小现场：

- 终端里最后一条测试输出
- 当前运行的内核版本/构建时间
- 当前 GDB 端口

如果系统已经 panic，先按普通崩溃路径排查；本 skill 主要用于“系统还活着，但线程睡死/事件没继续推进”的场景。

### 2. 优先做现场采样，不要先堆日志

挂住时先用 GDB 打断，确认：

1. CPU 是否都在 `idle`
2. 是否有 CPU 落在中断/softirq/workqueue/poll/锁竞争路径
3. 是否存在明显的 busy-loop / spin

如果两个 CPU 都 idle，通常说明：

- 不是锁自旋
- 不是网络线程忙等
- 更像“等待者睡了，但最后一次推进/唤醒没有发生”

### 3. 在最小阻塞点埋原子快照

不要一开始就加大范围过程日志。优先在真正会睡眠的点记录静态原子量。

优先放在真正会阻塞的边界上：

- 进入 `wait_event_*` / `futex` / `completion` / `schedule()` 前
- 重试循环准备真正睡眠前
- 事件从上层交给下层、下层再返还给上层的边界
- 你怀疑“吞掉事件/条件位/唤醒”的最窄位置

推荐记录字段：

- `ACTIVE`
- `PID`
- `TOTAL`
- `TARGET`
- `EVENTS`
- `STATE`
- `FLAGS`
- `QUEUE_LEN`
- `PROGRESS_COUNTER`
- `LAST_EVENT`
- `LAST_SOURCE_STATE`
- `LAST_TARGET_STATE`
- `TIMER_OR_DEADLINE`

命名建议：

- 使用 `DRAGONOS_<SUBSYSTEM>_DEBUG_<FIELD>` 风格
- 例如 `DRAGONOS_VFS_DEBUG_READ_WAIT_ACTIVE`
- 便于 `nm -C` 和 `gdb x/gx` 直接定位

实现建议：

- 使用 `AtomicUsize` / `AtomicU64`
- 默认用 `Ordering::Relaxed`
- 在 wait 前置 `ACTIVE=1`
- wait 返回后清掉 `ACTIVE`
- 快照函数只做“读状态 + 写原子”，不要顺手打印日志

### 4. 必要时在下层子系统加计数器

当 wait 点已经能证明“线程睡住了”，再向下一层加计数器，而不是直接打印过程日志。

常见有效计数器类型：

- 生产者/消费者计数：`enqueue/dequeue`、`wake/sleep`、`submit/complete`
- 队列状态：`queue_len`、`head/tail`、`depth/max_depth`
- 调度状态：`schedule_count`、`wake_count`、`sleep_count`
- 定时推进：`timer_fire_count`、`next_deadline`
- 设备推进：`irq_count`、`poll_count`、`rx_count`、`tx_count`

用计数器回答这些问题：

1. 事件有没有真的进入下一层
2. 下一层有没有继续消费/完成
3. 调度者/worker/中断有没有被触发
4. 触发后是没运行，还是运行了但没有交回结果

### 5. 重新编译并确认新内核真的在跑

Heisenbug 排查里，一个常见误区是：

- 代码改了
- 内核也编了
- 但当前 QEMU 还是旧实例

因此每次复现前都要确认：

1. 启动 banner 里的版本号/构建时间已经变化
2. `bin/vmstate/gdb` 对应的是当前实例
3. 不是在读上一个挂住实例的旧快照

### 6. 读取快照，而不是继续猜

读取命令模板见：

- [references/gdb-snapshot-cheatsheet.md](references/gdb-snapshot-cheatsheet.md)

读取时优先关注“证据链”，不要只看单个字段。

### 7. 用现场证据缩小层级

常见判读模式：

#### 模式 A

- `WAIT_ACTIVE=1`
- 关键等待条件仍不满足
- 队列为空或源事件计数不增长
- CPU 全 idle

含义：

- 真正睡住的是等待者
- 没有新的生产者在推进
- 优先向下一层继续看“事件是否真的产生”

#### 模式 B

- 上层等待中
- 下层 `queue_len > 0` 或 `produce_count > consume_count`

含义：

- 事件已经产生，但没有被及时继续消费
- 优先检查返回值语义、继续推进条件、worker 自驱动、队列可见性

#### 模式 C

- 下层计数正常增长
- 生产者和消费者计数基本匹配
- 但上层 syscall / API 仍在错误阻塞

含义：

- 数据路径可能已经走通
- 问题可能在更高层语义，例如：
  - 一个用户态 syscall 被错误拆成多个底层阻塞操作
  - 上层 retry/chunking 改坏了原本“一次调用一次阻塞点”的契约
  - 唤醒条件与返回条件不一致

这时应立即对比 Linux 6.6 或 DragonOS 自身的契约边界，而不是继续堆下层补丁。

### 8. 输出结果时保留“证伪链”

最终输出不要只写最后一个修复点。应同时记录：

1. 哪些方向被证伪
2. 哪些中间修复虽然不是最终一击，但修掉了真实竞态
3. 哪条现场证据迫使判断转向
4. 最终为什么认为某个修复符合 Linux 语义

## 语义对照提醒

遇到边界层时，优先检查：

- Linux/DragonOS 是否约定“一次上层调用只做一次底层阻塞操作”
- 返回条件和唤醒条件是否一致
- backlog/low-watermark/deadline 满足后是否应直接返回
- 某个“优化分块/重试/合并”是否改变了原始语义

## 附加资源

- 现场取证命令模板见 [references/gdb-snapshot-cheatsheet.md](references/gdb-snapshot-cheatsheet.md)
