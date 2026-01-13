:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/sched/wait_queue.md

- Translation time: 2026-01-13 06:32:49

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS Wait Queue Mechanism

## 1. Overview

The DragonOS WaitQueue is a process synchronization primitive based on the Waiter/Waker pattern, designed for process blocking and waking. Through atomic operations and a carefully designed wait-wake protocol, it completely solves the "wake loss" problem.

### 1.1 Core Features

- **Zero wake loss**: Ensures no wake signals are missed through the "register before check" mechanism
- **Single-use Waiter/Waker**: Creates a pair of objects for each wait to avoid state reuse issues
- **wait_until as core**: Builds on the `wait_until` API that returns resources as the foundation, with other APIs implemented upon it
- **Atomic wait and acquire**: Supports atomically "waiting for condition and acquiring resource" (e.g., lock Guards)
- **Multi-core friendly**: Wakers can be shared across CPUs, supporting concurrent wakeups
- **Signal awareness**: Supports both interruptible and uninterruptible wait modes

## 2. Core Design

### 2.1 Waiter/Waker Pattern

The wait queue adopts a producer-consumer model, separating waiters and wakers:

```rust
pub struct Waiter {
    waker: Arc<Waker>,
    _nosend: PhantomData<Rc<()>>,  // 标记为 !Send
}

pub struct Waker {
    has_woken: AtomicBool,    // 唤醒标志
    target: Weak<PCB>,        // 目标进程
}
```

**Key Features**:
- **Waiter**: Thread-local held (`!Send`/`!Sync`), can only wait on the thread that created it
- **Waker**: Shared through `Arc`, can be passed and used to wake across CPUs/threads
- **Single-use design**: Creates new Waiter/Waker pairs for each wait to prevent state pollution

### 2.2 Wait Queue Structure

```rust
pub struct WaitQueue {
    inner: SpinLock<InnerWaitQueue>,
    num_waiters: AtomicU32,  // 快速路径检查
}

struct InnerWaitQueue {
    dead: bool,                    // 队列失效标志
    waiters: VecDeque<Arc<Waker>>, // 等待者队列（FIFO）
}
```

**Design Highlights**:
- **Fast path optimization**: The atomic counter `num_waiters` allows lock-free checking of whether the queue is empty
- **FIFO order**: Uses `VecDeque` to ensure fairness
- **Death marker**: The `dead` flag is used for cleanup during resource destruction
- **Spinlock protection**: Uses `SpinLock` rather than `Mutex` to avoid recursive dependencies

## 3. Core API: wait_until Family

### 3.1 wait_until: Core Wait Primitive

`wait_until` is the core of the entire wait queue mechanism, with all other wait methods built upon it:

```rust
pub fn wait_until<F, R>(&self, cond: F) -> R
where
    F: FnMut() -> Option<R>,
{
    // 1. 快速路径：先检查条件
    if let Some(res) = cond() {
        return res;
    }

    // 2. 创建唯一的 Waiter/Waker 对
    let (waiter, waker) = Waiter::new_pair();

    loop {
        // 3. 先注册 waker 到队列（关键！）
        self.register_waker(waker.clone());

        // 4. 再检查条件（防止唤醒丢失）
        if let Some(res) = cond() {
            self.remove_waker(&waker);
            return res;
        }

        // 5. 睡眠等待被唤醒
        waiter.wait();

        // 6. 被唤醒后循环继续（可能是伪唤醒或竞争失败）
    }
}
```

**Key Design Concepts**:
1. **Enqueue before check**: Ensures any wake between condition check and sleep is properly handled
2. **Returns resource rather than boolean**: Returns `Option<R>` rather than `bool`, allowing direct return of acquired resources (e.g., lock Guards)
3. **Loop retry**: May fail competition after being woken, requiring re-enqueueing and rechecking
4. **Single Waiter/Waker pair**: Creates only one pair throughout the wait process, simplifying lifecycle management

### 3.2 wait_until Variants

```rust
// 不可中断等待
pub fn wait_until<F, R>(&self, cond: F) -> R

// 可中断等待（可被信号中断）
pub fn wait_until_interruptible<F, R>(&self, cond: F)
    -> Result<R, SystemError>

// 带超时的可中断等待
pub fn wait_until_timeout<F, R>(&self, cond: F, timeout: Duration)
    -> Result<R, SystemError>
```

**Return Value Semantics**:
- `Ok(R)`: Condition met, returns acquired resource
- `Err(ERESTARTSYS)`: Interrupted by signal
- `Err(EAGAIN_OR_EWOULDBLOCK)`: Timeout

### 3.3 Why wait_until is Superior to wait_event

Traditional `wait_event` series APIs only return a boolean:

```rust
// 传统方式（存在竞态）
wait_event(|| lock.is_available());
let guard = lock.acquire();  // ❌ 可能失败！另一个线程可能抢先获取
```

While `wait_until` can atomically "wait and acquire":

```rust
// wait_until 方式（无竞态）
let guard = wait_until(|| lock.try_acquire());  // ✅ 原子获取
```

**Key Advantages**:
- Eliminates the race window between "check and acquire"
- More concise code with clearer semantics
- Better performance (reduces one atomic operation)

## 4. Wait and Wake Flows

### 4.1 Detailed Wait Flow

```
┌─────────────────────┐
│  调用 wait_until    │
└──────────┬──────────┘
           │
           ↓
    ┌──────────────┐
    │ 快速路径：    │
    │ 检查条件      │
    └──────┬───────┘
           │
      满足? ────────┐
        │ 否       │ 是
        ↓          ↓
  ┌──────────┐  返回结果
  │ 创建      │
  │ Waiter/  │
  │ Waker    │
  └────┬─────┘
       │
       ↓ [循环开始]
  ┌──────────────┐
  │ 1. 注册 waker │ ← 关键：先入队
  │    到队列     │
  └──────┬───────┘
         │
         ↓
  ┌──────────────┐
  │ 2. 再次检查  │ ← 防止唤醒丢失
  │    条件       │
  └──────┬───────┘
         │
    满足? ────────┐
      │ 否       │ 是
      ↓          ↓
  ┌──────────┐ ┌──────────┐
  │ 检查信号 │ │ 移除      │
  │ (可中断) │ │ waker    │
  └────┬─────┘ └────┬─────┘
       │            │
  有信号? ──┐       ↓
    │ 否  │ 是   返回结果
    ↓     ↓
  ┌────┐ 返回
  │睡眠│ ERESTARTSYS
  └─┬──┘
    │
    ↓ [被唤醒]
  ┌──────────────┐
  │ 检查信号     │
  │ (可中断)     │
  └──────┬───────┘
         │
         ↓ [循环继续]
```

**Key Points Explained**:

1. **Enqueue before check**:
   - If checking before enqueuing, a wake could occur after the check returns false but before enqueueing
   - The wake signal would be lost as the waker isn't yet enqueued
   - Enqueuing first ensures any subsequent wake can find our waker

2. **Necessity of loop retry**:
   - Being woken doesn't guarantee the condition is met (could be spurious wakeup)
   - Even if the condition was met, competition might make it unmet again
   - Thus requires re-enqueueing and rechecking

3. **Advantages of single-use Waker**:
   - Avoids complex state management
   - Won't be woken again after being woken (`has_woken` flag)
   - Will re-register new waker in loops but using the same `Arc<Waker>`

### 4.2 Wake Flow

#### wake_one: Wake One Waiter

```rust
pub fn wake_one(&self) -> bool {
    // 快速路径：队列为空
    if self.is_empty() {
        return false;
    }

    loop {
        // 从队列头部取出一个 waker
        let next = {
            let mut guard = self.inner.lock_irqsave();
            let waker = guard.waiters.pop_front();
            if waker.is_some() {
                self.num_waiters.fetch_sub(1, Ordering::Release);
            }
            waker
        };

        let Some(waker) = next else { return false };

        // 释放锁后再唤醒（减少锁持有时间）
        if waker.wake() {
            return true;  // 成功唤醒
        }
        // waker 已失效（进程已退出），继续尝试下一个
    }
}
```

**Design Points**:
- **FIFO order**: Takes from queue head to ensure fairness
- **Wake outside lock**: Calls `waker.wake()` after releasing lock to reduce lock contention
- **Automatically skips invalid wakers**: If target process has exited, automatically tries next

#### wake_all: Wake All Waiters

```rust
pub fn wake_all(&self) -> usize {
    // 快速路径：队列为空
    if self.is_empty() {
        return 0;
    }

    // 一次性取出所有 waker（减少锁持有时间）
    let wakers = {
        let mut guard = self.inner.lock_irqsave();
        let mut drained = VecDeque::new();
        mem::swap(&mut guard.waiters, &mut drained);
        self.num_waiters.store(0, Ordering::Release);
        drained
    };

    // 释放锁后逐个唤醒
    let mut woken = 0;
    for waker in wakers {
        if waker.wake() {
            woken += 1;
        }
    }
    woken
}
```

**Design Points**:
- **Batch removal**: Clears queue at once to minimize lock holding time
- **Wake outside lock**: Wakes individually after releasing lock, allowing immediate competition
- **Returns actual wake count**: Distinguishes between "wake request" and "actual wake"

### 4.3 Waker Wake Mechanism

```rust
impl Waker {
    pub fn wake(&self) -> bool {
        // 原子设置唤醒标志
        if self.has_woken.swap(true, Ordering::Release) {
            return false;  // 已被唤醒过
        }

        // 唤醒目标进程
        if let Some(pcb) = self.target.upgrade() {
            ProcessManager::wakeup(&pcb);
        }
        true
    }

    pub fn close(&self) {
        // 关闭 waker，防止后续唤醒
        self.has_woken.store(true, Ordering::Acquire);
    }

    fn consume_wake(&self) -> bool {
        // 消费唤醒标志（用于 block_current）
        self.has_woken.swap(false, Ordering::Acquire)
    }
}
```

**Key Features**:
- **Atomic wake flag**: `has_woken` uses `AtomicBool` to ensure single wake
- **Weak reference to target process**: Uses `Weak<PCB>` to avoid circular references, auto-cleaning when process exits
- **Memory ordering guarantees**: Release/Acquire semantics ensure modifications before wake are visible to woken process

### 4.4 Blocking Current Process

```rust
fn block_current(waiter: &Waiter, interruptible: bool) -> Result<(), SystemError> {
    loop {
        // 快速路径：已被唤醒
        if waiter.waker.consume_wake() {
            return Ok(());
        }

        // 禁用中断，进入临界区
        let irq_guard = unsafe { CurrentIrqArch::save_and_disable_irq() };

        // 再次检查唤醒标志（处理"先唤后睡"竞态）
        if waiter.waker.consume_wake() {
            drop(irq_guard);
            return Ok(());
        }

        // 标记进程为睡眠状态
        ProcessManager::mark_sleep(interruptible)?;
        drop(irq_guard);  // 恢复中断

        // 调度到其他进程
        schedule(SchedMode::SM_NONE);

        // 被唤醒后，检查信号（可中断模式）
        if interruptible && Signal::signal_pending_state(...) {
            return Err(SystemError::ERESTARTSYS);
        }
    }
}
```

**Key Design**:
- **Double check**: Checks wake flag before and after interrupt disable to prevent "wake then sleep"
- **Interrupt protection**: `mark_sleep` must be executed with interrupts disabled
- **Signal check**: Checks for signals needing handling after being scheduled back

## 5. Memory Ordering and Correctness

### 5.1 Memory Ordering Guarantees

The wait queue uses the following memory ordering to ensure correctness:

| Operation | Memory Order | Purpose |
|-----------|--------------|---------|
| `waker.wake()` | Release | Ensures modifications before wake are visible to woken |
| `waiter.wait()` | Acquire | Ensures visibility of all modifications before wake |
| `register_waker` | Release | Ensures visibility of enqueue operation |
| `num_waiters` | Acquire/Release | Synchronizes counter modifications |

### 5.2 Happens-Before Relationships

```
线程 A（唤醒方）              线程 B（等待方）
    │                           │
    │ 修改共享数据               │
    │                           │
    ↓                           ↓
wake() (Release)          register_waker()
    │                           │
    │ happens-before             │
    │ ─────────────────────────→ │
    │                           ↓
    │                      wait() (Acquire)
    │                           │
    │                           ↓
    │                      观察到共享数据修改
```

**Guarantees**:
- All modifications by the waker before calling `wake()` are visible to the waiter
- This is the foundation for correct synchronization

### 5.3 Race-Free Proof

**Case 1: Enqueue then wake (normal flow)**
```
时间轴：
T1: 等待方 register_waker()
T2: 等待方检查条件，返回 false
T3: 等待方准备睡眠
T4: 唤醒方 wake_one()          ← waker 在队列中，正常唤醒
T5: 等待方被唤醒
```

**Case 2: Wake then enqueue (race to handle)**
```
时间轴：
T1: 等待方检查条件，返回 false
T2: 唤醒方修改条件为 true
T3: 唤醒方 wake_one()          ← waker 还未入队，唤醒失败
T4: 等待方 register_waker()
T5: 等待方再次检查条件         ← 检测到 true，不会睡眠！
```

**Key Point**: Through "check after enqueue", even if wake occurs before enqueue, the second check can detect the condition is already met.

## 6. Compatible APIs: wait_event Family

For backward compatibility, provides `wait_event` series APIs implemented based on `wait_until`:

```rust
// 可中断等待，返回 Result<(), SystemError>
pub fn wait_event_interruptible<F, B>(
    &self,
    mut cond: F,
    before_sleep: Option<B>,
) -> Result<(), SystemError>
where
    F: FnMut() -> bool,
    B: FnMut(),
{
    self.wait_until_impl(
        || if cond() { Some(()) } else { None },
        true,
        None,
        before_sleep,
    )
}

// 不可中断等待
pub fn wait_event_uninterruptible<F, B>(
    &self,
    mut cond: F,
    before_sleep: Option<B>,
) -> Result<(), SystemError>
where
    F: FnMut() -> bool,
    B: FnMut(),
{
    self.wait_until_impl(
        || if cond() { Some(()) } else { None },
        false,
        None,
        before_sleep,
    )
}

// 带超时的可中断等待
pub fn wait_event_interruptible_timeout<F>(
    &self,
    mut cond: F,
    timeout: Option<Duration>,
) -> Result<(), SystemError>
where
    F: FnMut() -> bool,
{
    self.wait_until_impl(
        || if cond() { Some(()) } else { None },
        true,
        timeout,
        None::<fn()>,
    )
}
```

**before_sleep Hook**:
- Executed after enqueueing and before sleeping
- Typical use: release locks to avoid sleeping while holding them
- Example: `wait_event_interruptible(|| cond(), Some(|| drop(guard)))`

## 7. Convenience Methods

### 7.1 Sleep and Release Lock

```rust
// 释放 SpinLock 并睡眠（可中断）
pub fn sleep_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>)
    -> Result<(), SystemError>

// 释放 Mutex 并睡眠（可中断）
pub fn sleep_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>)
    -> Result<(), SystemError>

// 释放 SpinLock 并睡眠（不可中断）
pub fn sleep_uninterruptible_unlock_spinlock<T>(&self, to_unlock: SpinLockGuard<T>)

// 释放 Mutex 并睡眠（不可中断）
pub fn sleep_uninterruptible_unlock_mutex<T>(&self, to_unlock: MutexGuard<T>)
```

**Usage Example**:
```rust
let guard = lock.lock();
// 检查条件
if !condition_met() {
    // 需要等待，释放锁并睡眠
    wait_queue.sleep_unlock_spinlock(guard)?;
    // 被唤醒后需要重新获取锁
}
```

### 7.2 Queue Lifecycle Management

```rust
// 标记队列失效，唤醒并清空所有等待者
pub fn mark_dead(&self) {
    let mut drained = VecDeque::new();
    {
        let mut guard = self.inner.lock_irqsave();
        guard.dead = true;
        mem::swap(&mut guard.waiters, &mut drained);
        self.num_waiters.store(0, Ordering::Release);
    }
    for w in drained {
        w.wake();
        w.close();
    }
}

// 检查队列是否为空
pub fn is_empty(&self) -> bool {
    self.num_waiters.load(Ordering::Acquire) == 0
}

// 获取等待者数量
pub fn len(&self) -> usize {
    self.num_waiters.load(Ordering::Acquire) as usize
}
```

## 8. Event Wait Queues

In addition to regular wait queues, provides event mask-based wait queues:

```rust
pub struct EventWaitQueue {
    wait_list: SpinLock<Vec<(u64, Arc<Waker>)>>,
}

impl EventWaitQueue {
    // 等待特定事件
    pub fn sleep(&self, events: u64)

    // 等待特定事件并释放锁
    pub fn sleep_unlock_spinlock<T>(&self, events: u64, to_unlock: SpinLockGuard<T>)

    // 唤醒等待任意匹配事件的线程（位掩码 AND）
    pub fn wakeup_any(&self, events: u64) -> usize

    // 唤醒等待精确匹配事件的线程（相等比较）
    pub fn wakeup(&self, events: u64) -> usize

    // 唤醒所有等待者
    pub fn wakeup_all(&self)
}
```

**Use Cases**:
- Waiting for multiple event types (e.g., `READABLE | WRITABLE`)
- Waking specific waiters by event type
- Example: socket poll/select implementation

## 9. Timeout Support

### 9.1 Timeout Mechanism

```rust
pub fn wait_until_timeout<F, R>(&self, cond: F, timeout: Duration)
    -> Result<R, SystemError>
where
    F: FnMut() -> Option<R>,
{
    self.wait_until_impl(cond, true, Some(timeout), None::<fn()>)
}
```

**Implementation Principle**:
1. Calculate deadline: `deadline = now + timeout`
2. Create timer to wake waiter upon expiration
3. After wake, check for timeout:
   - If timer triggered, return `EAGAIN_OR_EWOULDBLOCK`
   - If condition met, cancel timer and return result

### 9.2 TimeoutWaker

```rust
pub struct TimeoutWaker {
    waker: Arc<Waker>,
}

impl TimerFunction for TimeoutWaker {
    fn run(&mut self) -> Result<(), SystemError> {
        // 定时器到期，唤醒等待者
        self.waker.wake();
        Ok(())
    }
}
```

**Key Design**:
- Timer wakes through `Waker::wake()` rather than directly waking PCB
- Allows `Waiter::wait()` to correctly observe wake status
- Uses same mechanism as normal wake for consistency

## 10. Usage Examples

### 10.1 Semaphore Implementation

```rust
struct Semaphore {
    counter: AtomicU32,
    wait_queue: WaitQueue,
}

impl Semaphore {
    fn down(&self) -> Result<(), SystemError> {
        // 使用 wait_until 直接获取信号量
        self.wait_queue.wait_until(|| {
            let old = self.counter.load(Acquire);
            if old > 0 {
                // 尝试原子递减
                if self.counter.compare_exchange(
                    old, old - 1, Acquire, Relaxed
                ).is_ok() {
                    return Some(());  // 成功获取
                }
            }
            None  // 获取失败，继续等待
        });
        Ok(())
    }

    fn up(&self) {
        self.counter.fetch_add(1, Release);
        self.wait_queue.wake_one();
    }
}
```

**Advantages**:
- Uses `wait_until` to ensure atomic "wait and acquire"
- Avoids race window between "check and acquire"
- Clean and clear code

### 10.2 Condition Variable Implementation

```rust
struct CondVar {
    wait_queue: WaitQueue,
}

impl CondVar {
    fn wait<T>(&self, guard: MutexGuard<T>) -> Result<MutexGuard<T>, SystemError> {
        let mutex = guard.mutex();

        // 使用 before_sleep 钩子释放锁
        let mut guard = Some(guard);
        self.wait_queue.wait_event_interruptible(
            || false,  // 等待 notify
            Some(|| {
                if let Some(g) = guard.take() {
                    drop(g);  // 释放锁
                }
            }),
        )?;

        // 重新获取锁
        mutex.lock_interruptible()
    }

    fn notify_one(&self) {
        self.wait_queue.wake_one();
    }

    fn notify_all(&self) {
        self.wait_queue.wake_all();
    }
}
```

### 10.3 RwSem Integration

```rust
impl<T: ?Sized> RwSem<T> {
    pub fn read(&self) -> RwSemReadGuard<'_, T> {
        // 直接返回 Guard，无竞态
        self.queue.wait_until(|| self.try_read())
    }

    pub fn write(&self) -> RwSemWriteGuard<'_, T> {
        self.queue.wait_until(|| self.try_write())
    }

    pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError> {
        self.queue.wait_until_interruptible(|| self.try_read())
    }
}
```

**Why this design**:
- `try_read()` returns `Option<Guard>`, perfectly matching `wait_until` requirements
- One line implements complete "wait and acquire" logic
- Compiler ensures type safety, preventing forgotten checks

### 10.4 Timed Wait

```rust
fn wait_with_timeout(queue: &WaitQueue, timeout_ms: u64) -> Result<(), SystemError> {
    let timeout = Duration::from_millis(timeout_ms);
    queue.wait_event_interruptible_timeout(
        || condition_met(),
        Some(timeout),
    )
}
```

## 11. Performance Characteristics

### 11.1 Fast Path Optimization

- **No waiters**: `is_empty()` requires only one atomic read, no lock contention
- **Quick check**: `wait_until` first checks condition to avoid unnecessary enqueueing
- **Wake outside lock**: Wake operations occur after releasing queue lock to reduce lock holding time

### 11.2 Scalability

- **FIFO queue**: Ensures fairness, prevents starvation
- **Batch wake**: `wake_all()` removes all wakers at once to minimize lock contention
- **Cross-CPU wake**: Wakers can wake across CPUs without lock synchronization

### 11.3 Memory Overhead

- **Waiter**: Stack allocated, no heap involvement
- **Waker**: Shared through `Arc`, one per waiter
- **Queue**: Only occupies space when waiters exist, minimal overhead for empty queue

## 12. Comparison and Evolution

### 12.1 Comparison with Traditional Implementations

| Feature | Traditional wait_event | DragonOS wait_until |
|---------|------------------------|---------------------|
| API Return | `bool` | `Option<R>` |
| Atomic Acquire | Not supported (manual) | **Natively supported** |
| Wake Loss | Requires careful handling | **Design guarantees none** |
| Race Window | Check-acquire window | **No window** |
| Code Complexity | High (manual state management) | **Low (compiler guaranteed)** |
| Performance | May require multiple atomic ops | **Minimizes atomic ops** |

### 12.2 Design Evolution

**Old Design (Problematic)**:
```rust
// ❌ 可能存在唤醒丢失
loop {
    if condition() {
        return;
    }
    register_waker();
    sleep();
}
```

**Current Design (Correct)**:
```rust
// ✅ 无唤醒丢失
loop {
    register_waker();  // 先入队
    if condition() {   // 再检查
        remove_waker();
        return;
    }
    sleep();
}
```

## 13. Best Practices

### 13.1 Usage Recommendations

1. **Prefer wait_until**: Compared to `wait_event`, it provides stronger guarantees and cleaner code
2. **Leverage Option return value**: Directly return acquired resources to avoid secondary acquisition
3. **Properly use interruptible variants**: User-space processes should use `*_interruptible` to avoid inability to terminate
4. **Properly handle timeouts**: Distinguish between `ERESTARTSYS` (signal) and `EAGAIN_OR_EWOULDBLOCK` (timeout)

### 13.2 Pitfalls to Avoid

```rust
// ❌ 错误：分离检查和获取
if condition() {
    let guard = acquire();  // 可能失败！
}

// ✅ 正确：原子等待并获取
let guard = wait_until(|| try_acquire());

// ❌ 错误：忘记处理信号
let result = wait_queue.wait_event_interruptible(|| cond(), None);
// 未检查 result

// ✅ 正确：正确处理错误
let result = wait_queue.wait_event_interruptible(|| cond(), None)?;
```

### 13.3 Debugging Suggestions

- Use `wait_queue.len()` to check waiter count
- Monitor `has_woken` flag status (via logs)
- Check for processes blocked long-term (possible wake logic errors)

## 14. Implementation Principle Summary

```
                    ┌─────────────────────────────────────┐
                    │           WaitQueue                 │
                    │  ┌───────────────────────────────┐  │
                    │  │ inner: SpinLock<InnerQueue>  │  │
                    │  │   ├─ dead: bool              │  │
                    │  │   └─ waiters: VecDeque       │  │
                    │  └───────────────────────────────┘  │
                    │  ┌───────────────────────────────┐  │
                    │  │ num_waiters: AtomicU32       │  │
                    │  └───────────────────────────────┘  │
                    └─────────────────────────────────────┘
                                    │
                        ┌───────────┴───────────┐
                        ↓                       ↓
            ┌────────────────────┐  ┌────────────────────┐
            │      Waiter        │  │       Waker        │
            │  ┌──────────────┐  │  │  ┌──────────────┐  │
            │  │ waker: Arc   │──┼──┼─→│ has_woken:   │  │
            │  │              │  │  │  │  AtomicBool  │  │
            │  └──────────────┘  │  │  └──────────────┘  │
            │  ┌──────────────┐  │  │  ┌──────────────┐  │
            │  │ _nosend      │  │  │  │ target:      │  │
            │  │  PhantomData │  │  │  │  Weak<PCB>   │  │
            │  └──────────────┘  │  │  └──────────────┘  │
            └────────────────────┘  └────────────────────┘
                 (!Send)                   (Send+Sync)

核心流程：
┌──────────────────────────────────────────────────────────┐
│                   wait_until(cond)                       │
│                                                          │
│  1. 快速路径: if cond() → return                        │
│                                                          │
│  2. 创建 (waiter, waker) 对                             │
│                                                          │
│  3. loop {                                               │
│       ├─ register_waker(waker)      ← 先入队            │
│       ├─ if cond() → return         ← 再检查            │
│       ├─ waiter.wait()              ← 睡眠等待          │
│       └─ [被唤醒，循环继续]                             │
│     }                                                    │
└──────────────────────────────────────────────────────────┘

唤醒策略：
┌────────────────────┐
│    wake_one()      │  → 取出队首 waker → 唤醒一个进程
└────────────────────┘

┌────────────────────┐
│    wake_all()      │  → 取出所有 waker → 唤醒所有进程
└────────────────────┘

内存序保证：
    唤醒方: wake() (Release)
                │
                │ happens-before
                ↓
    等待方: wait() (Acquire)
```

This design achieves a zero wake loss, high-performance, easy-to-use wait queue mechanism through the core mechanisms of "enqueue before check", single-use Waiter/Waker pattern, and atomic memory ordering guarantees, providing a solid foundation for various synchronization primitives in DragonOS.
