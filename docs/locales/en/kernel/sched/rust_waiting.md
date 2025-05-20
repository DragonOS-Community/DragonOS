:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/sched/rust_waiting.md

- Translation time: 2025-05-19 01:41:21

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# APIs Related to "Waiting" (Rust Language)

&emsp;&emsp; If several processes need to wait for a certain event to occur before they can be executed, a "waiting" mechanism is required to achieve process synchronization.

## 1. WaitQueue Waiting Queue

&emsp;&emsp; WaitQueue is a process synchronization mechanism, known as "Waiting Queue" in Chinese. It can suspend the current process and, when the time is ripe, another process can wake them up.

&emsp;&emsp; When you need to wait for an event to complete, using the WaitQueue mechanism can reduce the overhead of process synchronization. Compared to abusing spinlocks and semaphores, or repeatedly using functions like usleep(1000), WaitQueue is an efficient solution.

### 1.1 Using WaitQueue

&emsp;&emsp; Using WaitQueue is very simple, requiring just three steps:

1. Initialize a WaitQueue object.
2. Call the API related to suspending the current process and suspend it.
3. When the event occurs, another process calls the API related to waking up the WaitQueue to wake up a process.

&emsp;&emsp; Here is a simple example:

### 1.1.1 Initializing a WaitQueue Object

&emsp;&emsp; Initializing a WaitQueue object is very simple; you just need to call `WaitQueue::INIT`.

```rust
let mut wq = WaitQueue::INIT;
```

### 1.1.2 Suspending the Process

&emsp;&emsp; You can suspend the current process as follows:

```rust
wq.sleep();
```

&emsp;&emsp; The current process will be suspended until another process calls `wq.wakeup()`.

### 1.1.3 Waking Up the Process

&emsp;&emsp; You can wake up a process as follows:

```rust
// 唤醒等待队列头部的进程（如果它的state & PROC_INTERRUPTIBLE 不为0）
wq.wakeup(PROC_INTERRUPTIBLE);

// 唤醒等待队列头部的进程（如果它的state & PROC_UNINTERRUPTIBLE 不为0）
wq.wakeup(PROC_UNINTERRUPTIBLE);

// 唤醒等待队列头部的进程（无论它的state是什么）
wq.wakeup((-1) as u64);
```

### 1.2 APIs

### 1.2.1 Suspending the Process

&emsp;&emsp; You can use the following functions to suspend the current process and insert it into the specified waiting queue. These functions have similar overall functionality, but differ in some details.

| Function Name                             | Explanation                                                       |
| --------------------------------------- | ---------------------------------------------------------------- |
| sleep()                                 | Suspend the current process and set its state to `PROC_INTERRUPTIBLE` |
| sleep_uninterruptible()                 | Suspend the current process and set its state to `PROC_UNINTERRUPTIBLE` |
| sleep_unlock_spinlock()                 | Suspend the current process and set its state to `PROC_INTERRUPTIBLE`. After inserting the current process into the waiting queue, unlock the given spinlock |
| sleep_unlock_mutex()                    | Suspend the current process and set its state to `PROC_INTERRUPTIBLE`. After inserting the current process into the waiting queue, unlock the given Mutex |
| sleep_uninterruptible_unlock_spinlock() | Suspend the current process and set its state to `PROC_UNINTERRUPTIBLE`. After inserting the current process into the waiting queue, unlock the given spinlock |
| sleep_uninterruptible_unlock_mutex()    | Suspend the current process and set its state to `PROC_UNINTERRUPTIBLE`. After inserting the current process into the waiting queue, unlock the given Mutex |

### 1.2.2 Waking Up the Process

&emsp;&emsp; You can use the `wakeup(state)` function to wake up the first process in the waiting queue. If the process's state, after performing a bitwise AND operation with the given state, results in a non-zero value, it will be woken up.

&emsp;&emsp; Return value: Returns `true` if a process is woken up, otherwise returns `false`.

### 1.2.3 Other APIs

| Function Name | Explanation         |
| ------------- | ------------------ |
| len()         | Returns the number of processes in the waiting queue |
