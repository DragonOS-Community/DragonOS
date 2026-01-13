# DragonOS Workqueue 机制设计文档

## 1. 概述

工作队列（Workqueue）是 DragonOS 内核中的一种"下半部"（Bottom Half）机制，用于将任务推迟到**进程上下文**中执行。与 Softirq 和 Tasklet 不同，工作队列的处理函数运行在内核线程中，因此允许执行以下操作：
- 睡眠（阻塞等待资源）
- 获取互斥锁（Mutex）或信号量
- 执行 I/O 操作
- 分配内存（可能导致阻塞）

本机制的设计参考了 Linux 内核的工作队列实现，并针对 DragonOS 的 Rust 架构进行了适配。

## 2. 核心架构

### 2.1 核心组件

Workqueue 机制主要由以下三个组件构成：

1.  **Work（工作项）**：
    - 定义了需要延迟执行的具体任务。
    - 在 Rust 实现中，本质上是一个封装了闭包（Closure）或函数指针的结构体。
    - 通过 `Arc` 进行引用计数管理，支持跨线程共享。

2.  **WorkQueue（工作队列）**：
    - 负责管理待处理的工作项（Pending Works）。
    - 维护一个或多个关联的**工作线程（Worker Thread）**。
    - 内部包含一个 FIFO 队列和一个 `WaitQueue`（用于线程同步）。

3.  **Worker Thread（工作线程）**：
    - 一个专门的内核线程，循环从 WorkQueue 中取出任务并执行。
    - 当队列为空时，线程进入睡眠状态（通过 `WaitQueue` 挂起）。
    - 当有新任务入队时，线程被唤醒。

### 2.2 数据流图

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

## 3. 详细设计

### 3.1 数据结构设计

#### Work
`Work` 结构体封装了具体的任务逻辑。使用 `Box<dyn Fn() + Send + Sync>` 来存储闭包，确保其可以在线程间安全传递和执行。

```rust
pub struct Work {
    func: Box<dyn Fn() + Send + Sync>,
}
```

#### WorkQueue
`WorkQueue` 是核心管理结构。它包含：
- `queue`: 一个受自旋锁（SpinLock）保护的双端队列，存储 `Arc<Work>`。
- `wait_queue`: 用于工作线程的同步等待。
- `worker`: 指向关联内核线程 PCB 的引用。

```rust
pub struct WorkQueue {
    queue: SpinLock<VecDeque<Arc<Work>>>,
    wait_queue: Arc<WaitQueue>,
    worker: SpinLock<Option<Arc<ProcessControlBlock>>>,
}
```

### 3.2 运行机制

1.  **初始化**：
    - 调用 `WorkQueue::new(name)` 创建一个新的工作队列。
    - 该操作会自动启动一个名为 `name` 的内核线程。

2.  **任务调度**：
    - 调用者使用 `Work::new(closure)` 创建工作项。
    - 调用 `wq.enqueue(work)` 将任务加入队列。
    - `enqueue` 操作会将任务推入队列尾部，并调用 `wait_queue.wakeup()` 唤醒工作线程。

3.  **任务执行**：
    - 工作线程运行 `worker_loop` 函数。
    - 循环检查队列状态：
        - 若队列非空：取出队首任务，调用 `work.run()` 执行。
        - 若队列为空：在 `wait_queue` 上调用 `wait_event_interruptible` 进入睡眠，等待被唤醒。

### 3.3 系统全局队列 (`SYSTEM_WQ`)

为了简化常见场景的使用，系统提供了一个全局默认工作队列 `SYSTEM_WQ`。
- 大多数简单的后台任务可以直接提交到该队列，无需创建专用的 WorkQueue。
- 通过 `schedule_work(work)` 接口直接调度。

## 4. 接口说明

### 4.1 创建任务

```rust
let work = Work::new(|| {
    log::info!("This is running in a workqueue!");
    // 可以安全地睡眠或加锁
});
```

### 4.2 调度任务

```rust
// 调度到系统默认队列
schedule_work(work);

// 或者调度到自定义队列
let my_wq = WorkQueue::new("my_driver_wq");
my_wq.enqueue(work);
```

## 5. 与 Linux 的异同

- **相同点**：
    - 都是基于内核线程的延迟执行机制。
    - 都支持睡眠和阻塞操作。
    - 都提供了系统默认的全局队列。

- **不同点**：
    - **并发模型**：DragonOS 当前实现采用"单线程单队列"模型（每个 WorkQueue 对应一个内核线程）。Linux 采用复杂的 Worker Pool 模型，支持动态扩缩容和并发管理（CM）。
    - **Per-CPU**：DragonOS 当前未实现 Per-CPU 的 WorkQueue，所有任务在同一个线程队列中处理。
    - **API 风格**：DragonOS 利用 Rust 的闭包特性，使得任务定义更加灵活简洁，不需要像 Linux 那样嵌入 `work_struct` 到自定义结构体中（尽管也支持类似模式）。

## 6. 未来演进

1.  **并发管理**：引入 Worker Pool 机制，允许多个 Worker 消费同一个队列，提高并发度。
2.  **Per-CPU 队列**：实现 Per-CPU 的 WorkQueue，减少锁竞争，提高缓存局部性。
3.  **延迟工作（Delayed Work）**：集成定时器机制，支持 `schedule_delayed_work`，允许任务在指定延迟后执行。
