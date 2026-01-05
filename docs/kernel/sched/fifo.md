# FIFO调度器

&emsp;&emsp; FIFO（First-In-First-Out）调度器是DragonOS中实现的一种实时调度策略。FIFO调度器采用先进先出的调度算法，为实时任务提供确定性的调度行为。

## 1. 设计概述

&emsp;&emsp; FIFO调度器是为实时任务设计的，其核心特点是：

1. **无时间片机制**：FIFO任务一旦获得CPU，将一直运行直到主动释放CPU或被更高优先级的任务抢占
2. **优先级调度**：支持0-99共100个优先级，数字越小优先级越高
3. **同优先级FIFO**：相同优先级的任务严格按照入队顺序执行

## 2. 数据结构

### 2.1 FifoRunQueue

&emsp;&emsp; `FifoRunQueue`是FIFO调度器的运行队列，每个CPU维护一个实例。

```rust
pub struct FifoRunQueue {
    queues: Vec<VecDeque<Arc<ProcessControlBlock>>>,  // 100个优先级队列
    active: u128,                                      // 优先级位图，快速查找最高优先级
    nr_running: usize,                                 // 运行队列中的进程数
}
```

**设计要点：**

- **多级队列**：使用100个`VecDeque`分别存储不同优先级的进程
- **位图优化**：`active`字段使用128位位图记录哪些优先级队列非空，通过`trailing_zeros()`指令快速定位最高优先级
- **O(1)选择**：利用位图和双端队列，`pick_next()`操作达到O(1)时间复杂度

### 2.2 FifoScheduler

&emsp;&emsp; `FifoScheduler`实现了`Scheduler` trait，提供FIFO调度策略的核心逻辑。

## 3. 已实现功能

### 3.1 基础调度操作

| 函数 | 功能 | 实现状态 |
|------|------|----------|
| `enqueue()` | 将进程加入调度队列 | ✅ 已实现 |
| `dequeue()` | 将进程从调度队列移除 | ✅ 已实现 |
| `pick_next_task()` | 选择下一个要执行的进程 | ✅ 已实现 |
| `yield_task()` | 当前进程主动让出CPU | ✅ 已实现 |

### 3.2 抢占机制

**check_preempt_currnet()**：当有新进程被唤醒时，检查是否需要抢占当前进程
- 如果新进程优先级更高，触发抢占
- 支持实时任务与普通任务之间的抢占

**tick()**：时钟中断处理
- 检查是否有更高优先级的任务进入队列
- 如有则触发重新调度

### 3.3 调度优先级

FIFO调度器使用与Linux兼容的优先级范围：

```rust
pub const MAX_RT_PRIO: i32 = 100;  // 实时优先级范围 0-99
```

- 优先级0：最高优先级
- 优先级99：最低实时优先级
- 优先级>=100：普通进程（CFS调度）

### 3.4 策略切换

通过`ProcessManager::set_fifo_policy()`接口，支持运行时将内核线程切换为FIFO调度策略：

```rust
pub fn set_fifo_policy(pcb: &Arc<ProcessControlBlock>, prio: i32) -> Result<(), SystemError>
```

该函数会：
1. 验证进程必须是内核线程（KTHREAD标志）
2. 验证优先级在有效范围内（0-99）
3. 处理进程在运行队列中的状态变更
4. 触发抢占检查

## 4. 调度流程

### 4.1 进程入队

```
enqueue()
  ↓
计算进程优先级索引
  ↓
加入对应优先级队列尾部
  ↓
更新位图active
  ↓
nr_running++
```

### 4.2 选择下一进程

```
pick_next_task()
  ↓
从位图获取最高优先级（trailing_zeros）
  ↓
返回该优先级队列队首进程
```

### 4.3 抢占判断

```
新进程唤醒 / 时钟中断
  ↓
获取当前进程和新进程优先级
  ↓
if (新进程优先级 < 当前进程优先级):  // 数字越小优先级越高
  ↓
设置重调度标志
```

## 5. Demo功能

&emsp;&emsp; 通过`fifo_demo` feature可以启用演示功能（`kernel/src/sched/fifo_demo.rs`），该功能创建一个FIFO调度的内核线程：

- 设置CPU亲和性为Core 0
- 设置FIFO调度策略，优先级50
- 每5秒输出一次日志

启用方式：在`Cargo.toml`中添加feature并调用`fifo_demo_init()`

## 6. TODO

### 6.1 多核支持

- [ ] 实现多CPU之间的FIFO任务负载均衡
- [ ] 支持任务的CPU亲和性设置与迁移

### 6.2 调度增强

- [ ] 实现SCHED_RR（时间片轮转）调度策略
- [ ] 支持动态优先级调整
- [ ] 添加调度延迟统计和监控

### 6.3 实时性保障

- [ ] 实现实时任务带宽限制
- [ ] 添加优先级继承机制（防止优先级反转）
- [ ] 支持EDF（最早截止时间优先）调度策略

### 6.4 用户态接口

- [ ] 实现`sched_setscheduler`系统调用

### 6.5 优化与调试

- [ ] 添加FIFO调度器的调试信息输出
- [ ] 实现调度延迟监控接口
- [ ] 优化位图操作，支持更多优先级级数
