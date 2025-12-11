:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/sched/wait_queue.md

- Translation time: 2025-12-10 06:04:58

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS Wait Queue Mechanism

## Overview

The DragonOS wait queue is a synchronization primitive based on the Waiter/Waker pattern, used for process blocking and waking. It resolves the "wakeup lost" issue through atomic operations.

## Core Design

### Waiter/Waker Pattern

Each wait operation creates a pair of objects:

- **Waiter**: Thread-local (not Send/Sync), used for waiting
- **Waker**: Cross-CPU shared (Arc), used for waking

### Key Structures

```rust
struct WaitQueue {
    inner: SpinLock<VecDeque<Arc<Waker>>>,
    num_waiters: AtomicU32,  // 快速路径检查
}

struct Waker {
    has_woken: AtomicBool,    // 唤醒标记
    target: Weak<PCB>,        // 目标进程
}
```

## Wait and Wakeup Flow

### Wait Process

1. Check condition, return directly if met
2. Create Waiter/Waker pair, lock and add Waker to queue
3. Recheck condition (handle race)
4. Execute before_sleep hook (typically releases lock)
5. Call Waiter::wait() to block current process

```rust
// 等待条件满足
wait_queue.wait_event_interruptible(
    || condition_is_met(),
    Some(|| unlock_mutex()),
)?;
```

### Wakeup Process

```
┌─────────────────┐
│  wake_one()     │
└─────────────────┘
         │
         ▼
┌─────────────────┐
│ 检查队列是否空  │ (快速路径)
└─────────────────┘
         │
    空  │ │ 非空
        ▼ ▼
   返回  加锁队列
         │
         ▼
┌─────────────────┐
│ 取出一个 Waker  │
└─────────────────┘
         │
         ▼
┌─────────────────┐
│ 释放锁后唤醒    │
└─────────────────┘
         │
         ▼
┌─────────────────┐
│设置 has_woken   │
│并唤醒进程       │
└─────────────────┘
```

### Memory Ordering Guarantees

- **Waker side**: Uses Release semantics to set `has_woken`
- **Waiter side**: Uses Acquire semantics to read `has_woken`
- Ensures all operations before wakeup are visible to waiters

## Core Implementation

### Waker::wake

```rust
pub fn wake(&self) -> bool {
    // 原子设置唤醒标记
    if self.has_woken.swap(true, Ordering::Release) {
        return false;  // 已被唤醒
    }

    // 唤醒目标进程
    if let Some(pcb) = self.target.upgrade() {
        ProcessManager::wakeup(&pcb);
    }
    true
}
```

### block_current

```rust
fn block_current(waiter: &Waiter, interruptible: bool) -> Result<(), SystemError> {
    loop {
        // 快速路径：已被唤醒
        if waiter.waker.consume_wake() {
            return Ok(());
        }

        // 禁用中断，标记进程为睡眠状态
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };
        ProcessManager::mark_sleep(interruptible)?;
        drop(irq_guard);

        // 调度出去
        schedule(SchedMode::SM_NONE);

        // 检查信号（可中断等待）
        if interruptible && Signal::signal_pending_state(...) {
            return Err(SystemError::ERESTARTSYS);
        }
    }
}
```

## Main APIs

```rust
// 等待事件
wait_queue.wait_event_interruptible(|| condition_met, Some(|| unlock()))?;

// 唤醒
wait_queue.wakeup(None);     // 唤醒一个
wait_queue.wake_all();       // 唤醒所有

// 便利方法
wait_queue.sleep_unlock_spinlock(guard)?;  // 睡眠并释放锁
```

## Usage Example

### Semaphore

```rust
struct Semaphore {
    counter: AtomicU32,
    wait_queue: WaitQueue,
}

impl Semaphore {
    fn down(&self) -> Result<(), SystemError> {
        loop {
            // 尝试获取
            if self.counter.fetch_sub(1, Acquire) > 0 {
                return Ok(());
            }
            // 失败，等待
            self.counter.fetch_add(1, Release);
            self.wait_queue.wait_event_interruptible(
                || self.counter.load(Acquire) > 0,
                None,
            )?;
        }
    }

    fn up(&self) {
        self.counter.fetch_add(1, Release);
        self.wait_queue.wakeup(None);
    }
}
```

## Key Advantages

1. **No wakeup loss**: Atomic `has_woken` marking ensures this
2. **Simple to use**: Unified API, no manual state management needed
3. **High performance**: Fast path optimization, minimizes lock holding time
4. **Multi-core friendly**: Supports concurrent wakeups, reduces lock contention

This mechanism provides foundational support for DragonOS synchronization primitives (semaphores, condition variables, futex, etc.).
