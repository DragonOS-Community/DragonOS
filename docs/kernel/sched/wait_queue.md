# DragonOS 等待队列机制

## 概述

DragonOS 等待队列是基于 Waiter/Waker 模式的同步原语，用于进程阻塞等待和唤醒。通过原子操作解决"唤醒丢失"问题。

## 核心设计

### Waiter/Waker 模式

每次等待操作创建一对对象：

- **Waiter**：线程本地持有（!Send/!Sync），用于等待
- **Waker**：跨 CPU 共享（Arc），用于唤醒

### 关键结构

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

## 等待与唤醒流程

### 等待流程

1. 检查条件，满足则直接返回
2. 创建 Waiter/Waker 对，加锁将 Waker 加入队列
3. 再次检查条件（处理竞态）
4. 执行 before_sleep 钩子（通常释放锁）
5. 调用 Waiter::wait() 阻塞当前进程

```rust
// 等待条件满足
wait_queue.wait_event_interruptible(
    || condition_is_met(),
    Some(|| unlock_mutex()),
)?;
```

### 唤醒流程

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

### 内存序保证

- **唤醒方**：使用 Release 语义设置 `has_woken`
- **等待方**：使用 Acquire 语义读取 `has_woken`
- 保证唤醒前的所有操作对等待方可见

## 核心实现

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

## 主要 API

```rust
// 等待事件
wait_queue.wait_event_interruptible(|| condition_met, Some(|| unlock()))?;

// 唤醒
wait_queue.wakeup(None);     // 唤醒一个
wait_queue.wake_all();       // 唤醒所有

// 便利方法
wait_queue.sleep_unlock_spinlock(guard)?;  // 睡眠并释放锁
```

## 使用示例

### 信号量

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

## 关键优势

1. **无唤醒丢失**：原子 `has_woken` 标记确保
2. **简单易用**：统一的 API，无需手动管理状态
3. **高性能**：快速路径优化，最小化锁持有时间
4. **多核友好**：支持并发唤醒，减少锁竞争

这套机制为 DragonOS 的同步原语（信号量、条件变量、futex 等）提供了基础支撑。