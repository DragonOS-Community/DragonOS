:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/interrupt/workqueue.md

- Translation time: 2026-01-05 16:08:30

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS Workqueue Mechanism Design Document

## 1. Overview

The Workqueue is a "bottom half" mechanism in the DragonOS kernel used to defer tasks for execution in the **process context**. Unlike Softirq and Tasklet, the workqueue handler functions run in kernel threads, thereby allowing the following operations:
- Sleeping (blocking while waiting for resources)
- Acquiring mutexes or semaphores
- Performing I/O operations
- Allocating memory (which may block)

The design of this mechanism is based on the Linux kernel's workqueue implementation, adapted for DragonOS's Rust architecture.

## 2. Core Architecture

### 2.1 Core Components

The Workqueue mechanism primarily consists of the following three components:

1. **Work (Work Item)**:
   - Defines the specific task to be executed with a delay.
   - In the Rust implementation, it is essentially a structure encapsulating a closure or function pointer.
   - Managed via `Arc` for reference counting, supporting cross-thread sharing.

2. **WorkQueue (Work Queue)**:
   - Responsible for managing pending work items.
   - Maintains one or more associated **worker threads**.
   - Internally contains a FIFO queue and a `WaitQueue` (for thread synchronization).

3. **Worker Thread (Worker Thread)**:
   - A dedicated kernel thread that cyclically retrieves tasks from the WorkQueue and executes them.
   - When the queue is empty, the thread enters a sleep state (suspended via `WaitQueue`).
   - When new tasks are enqueued, the thread is awakened.

### 2.2 Data Flow Diagram

```mermaid
graph TD
    User[调用者 (Driver/Interrupt)] -->|schedule_work()| WQ[WorkQueue]
    WQ -->|enqueue| Queue[待处理队列]
    WQ -->|wakeup| Worker[Worker Thread]
    
    subgraph Worker Thread Loop
        Check{队列为空?} -->|Yes| Sleep[睡眠等待]
        Check -->|No| Dequeue[取出 Work]
        Dequeue --> Run[执行 Work.func()]
        Run --> Check
    end
    
    Sleep -.->|被唤醒| Check
```

## 3. Detailed Design

### 3.1 Data Structure Design

#### Work
The `Work` structure encapsulates the specific task logic. It uses `Box<dyn Fn() + Send + Sync>` to store closures, ensuring they can be safely transferred and executed across threads.

```rust
pub struct Work {
    func: Box<dyn Fn() + Send + Sync>,
}
```

#### WorkQueue
The `WorkQueue` is the core management structure. It includes:
- `queue`: A spinlock-protected deque storing `Arc<Work>`.
- `wait_queue`: Used for worker thread synchronization and waiting.
- `worker`: A reference pointing to the associated kernel thread's PCB.

```rust
pub struct WorkQueue {
    queue: SpinLock<VecDeque<Arc<Work>>>,
    wait_queue: Arc<WaitQueue>,
    worker: SpinLock<Option<Arc<ProcessControlBlock>>>,
}
```

### 3.2 Operational Mechanism

1. **Initialization**:
   - Calling `WorkQueue::new(name)` creates a new work queue.
   - This operation automatically starts a kernel thread named `name`.

2. **Task Scheduling**:
   - The caller uses `Work::new(closure)` to create a work item.
   - `wq.enqueue(work)` is called to enqueue the task.
   - The `enqueue` operation pushes the task to the end of the queue and calls `wait_queue.wakeup()` to awaken the worker thread.

3. **Task Execution**:
   - The worker thread runs the `worker_loop` function.
   - It cyclically checks the queue status:
     - If the queue is not empty: the head task is dequeued, and `work.run()` is called to execute it.
     - If the queue is empty: the thread sleeps on `wait_queue` by calling `wait_event_interruptible`, awaiting awakening.

### 3.3 System Global Queue (`SYSTEM_WQ`)

To simplify usage in common scenarios, the system provides a global default work queue `SYSTEM_WQ`.
- Most simple background tasks can be directly submitted to this queue without creating a dedicated WorkQueue.
- Tasks are scheduled directly via the `schedule_work(work)` interface.

## 4. Interface Description

### 4.1 Creating Tasks

```rust
let work = Work::new(|| {
    log::info!("This is running in a workqueue!");
    // 可以安全地睡眠或加锁
});
```

### 4.2 Scheduling Tasks

```rust
// 调度到系统默认队列
schedule_work(work);

// 或者调度到自定义队列
let my_wq = WorkQueue::new("my_driver_wq");
my_wq.enqueue(work);
```

## 5. Differences from Linux

- **Similarities**:
  - Both are delayed execution mechanisms based on kernel threads.
  - Both support sleeping and blocking operations.
  - Both provide a system-default global queue.

- **Differences**:
  - **Concurrency Model**: The current DragonOS implementation adopts a "single-thread single-queue" model (each WorkQueue corresponds to a kernel thread). Linux employs a complex Worker Pool model, supporting dynamic scaling and concurrency management (CM).
  - **Per-CPU**: DragonOS currently does not implement Per-CPU WorkQueues; all tasks are processed in the same thread queue.
  - **API Style**: DragonOS leverages Rust's closure features, making task definitions more flexible and concise. Unlike Linux, it does not require embedding `work_struct` into custom structures (though similar patterns are also supported).

## 6. Future Evolution

1. **Concurrency Management**: Introduce a Worker Pool mechanism to allow multiple workers to consume the same queue, improving concurrency.
2. **Per-CPU Queues**: Implement Per-CPU WorkQueues to reduce lock contention and enhance cache locality.
3. **Delayed Work (Delayed Work)**: Integrate timer mechanisms to support `schedule_delayed_work`, allowing tasks to execute after a specified delay.
