:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/sched/fifo.md

- Translation time: 2026-01-05 12:01:34

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# FIFO Scheduler

&emsp;&emsp; The FIFO (First-In-First-Out) scheduler is a real-time scheduling policy implemented in DragonOS. The FIFO scheduler adopts an advanced-first-out scheduling algorithm to provide deterministic scheduling behavior for real-time tasks.

## 1. Design Overview

&emsp;&emsp; The FIFO scheduler is designed for real-time tasks, with the following core characteristics:

1. **No time slice mechanism**: Once a FIFO task obtains the CPU, it will run continuously until it voluntarily releases the CPU or is preempted by a higher-priority task.
2. **Priority scheduling**: Supports 100 priorities from 0 to 99, where a smaller number indicates a higher priority.
3. **Same-priority FIFO**: Tasks with the same priority are executed strictly in the order they enter the queue.

## 2. Data Structures

### 2.1 FifoRunQueue

&emsp;&emsp; `FifoRunQueue` is the run queue of the FIFO scheduler, with one instance maintained per CPU.

```rust
pub struct FifoRunQueue {
    queues: Vec<VecDeque<Arc<ProcessControlBlock>>>,  // 100个优先级队列
    active: u128,                                      // 优先级位图，快速查找最高优先级
    nr_running: usize,                                 // 运行队列中的进程数
}
```

**Design Highlights:**

- **Multi-level queues**: Uses 100 `VecDeque` to store processes of different priorities separately.
- **Bitmap optimization**: The `active` field uses a 128-bit bitmap to record which priority queues are non-empty, quickly locating the highest priority through the `trailing_zeros()` instruction.
- **O(1) selection**: Utilizes the bitmap and deque to achieve O(1) time complexity for the `pick_next()` operation.

### 2.2 FifoScheduler

&emsp;&emsp; `FifoScheduler` implements the `Scheduler` trait, providing the core logic of the FIFO scheduling policy.

## 3. Implemented Features

### 3.1 Basic Scheduling Operations

| Function | Functionality | Implementation Status |
|----------|---------------|-----------------------|
| `enqueue()` | Add a process to the scheduling queue | ✅ Implemented |
| `dequeue()` | Remove a process from the scheduling queue | ✅ Implemented |
| `pick_next_task()` | Select the next process to execute | ✅ Implemented |
| `yield_task()` | Current process voluntarily yields the CPU | ✅ Implemented |

### 3.2 Preemption Mechanism

**check_preempt_currnet()**: When a new process is awakened, check whether the current process needs to be preempted.
- If the new process has a higher priority, trigger preemption.
- Supports preemption between real-time tasks and normal tasks.

**tick()**: Clock interrupt handling.
- Check whether a higher-priority task has entered the queue.
- If so, trigger rescheduling.

### 3.3 Scheduling Priority

The FIFO scheduler uses a priority range compatible with Linux:

```rust
pub const MAX_RT_PRIO: i32 = 100;  // 实时优先级范围 0-99
```

- Priority 0: Highest priority.
- Priority 99: Lowest real-time priority.
- Priority >=100: Normal processes (CFS scheduling).

### 3.4 Policy Switching

Through the `ProcessManager::set_fifo_policy()` interface, it supports switching kernel threads to the FIFO scheduling policy at runtime:

```rust
pub fn set_fifo_policy(pcb: &Arc<ProcessControlBlock>, prio: i32) -> Result<(), SystemError>
```

This function will:
1. Verify that the process must be a kernel thread (KTHREAD flag).
2. Verify that the priority is within the valid range (0-99).
3. Handle the state change of the process in the run queue.
4. Trigger preemption checks.

## 4. Scheduling Process

### 4.1 Process Enqueue

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

### 4.2 Selecting the Next Process

```
pick_next_task()
  ↓
从位图获取最高优先级（trailing_zeros）
  ↓
返回该优先级队列队首进程
```

### 4.3 Preemption Judgment

```
新进程唤醒 / 时钟中断
  ↓
获取当前进程和新进程优先级
  ↓
if (新进程优先级 < 当前进程优先级):  // 数字越小优先级越高
  ↓
设置重调度标志
```

## 5. Demo Functionality

&emsp;&emsp; The demo functionality can be enabled through the `fifo_demo` feature (`kernel/src/sched/fifo_demo.rs`), which creates a FIFO-scheduled kernel thread:
- Sets CPU affinity to Core 0.
- Sets the FIFO scheduling policy with priority 50.
- Outputs a log every 5 seconds.

Enabling method: Add the feature in `Cargo.toml` and call `fifo_demo_init()`.

## 6. TODO

### 6.1 Multi-core Support

- [ ] Implement FIFO task load balancing between multiple CPUs.
- [ ] Support setting and migrating CPU affinity for tasks.

### 6.2 Scheduling Enhancements

- [ ] Implement the SCHED_RR (round-robin) scheduling policy.
- [ ] Support dynamic priority adjustment.
- [ ] Add scheduling latency statistics and monitoring.

### 6.3 Real-time Guarantees

- [ ] Implement real-time task bandwidth limits.
- [ ] Add priority inheritance mechanism (to prevent priority inversion).
- [ ] Support EDF (Earliest Deadline First) scheduling policy.

### 6.4 User-space Interface

- [ ] Implement the `sched_setscheduler` system call.

### 6.5 Optimization and Debugging

- [ ] Add debugging information output for the FIFO scheduler.
- [ ] Implement a scheduling latency monitoring interface.
- [ ] Optimize bitmap operations to support more priority levels.
