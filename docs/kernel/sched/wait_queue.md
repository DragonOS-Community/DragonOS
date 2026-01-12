# DragonOS 等待队列机制

## 1. 概述

DragonOS 等待队列（WaitQueue）是基于 Waiter/Waker 模式的进程同步原语，用于进程阻塞等待和唤醒。它通过原子操作和精心设计的等待-唤醒协议，彻底解决了"唤醒丢失"问题。

### 1.1 核心特性

- **零唤醒丢失**：通过"先注册后检查"的机制确保不会错过任何唤醒信号
- **一次性 Waiter/Waker**：每次等待创建一对对象，避免状态复用问题
- **wait_until 为核心**：以返回资源的 `wait_until` 为基础 API，其他 API 基于此实现
- **原子等待与获取**：支持原子地"等待条件并获取资源"（如锁的 Guard）
- **多核友好**：Waker 可跨 CPU 共享，支持并发唤醒
- **信号感知**：支持可中断和不可中断的等待模式

## 2. 核心设计

### 2.1 Waiter/Waker 模式

等待队列采用生产者-消费者模式，将等待方和唤醒方分离：

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

**关键特性**：
- **Waiter**：线程本地持有（`!Send`/`!Sync`），只能在创建它的线程上等待
- **Waker**：通过 `Arc` 共享，可以跨 CPU/线程传递和唤醒
- **一次性设计**：每次等待创建新的 Waiter/Waker 对，避免状态污染

### 2.2 等待队列结构

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

**设计亮点**：
- **快速路径优化**：`num_waiters` 原子计数器允许无锁检查队列是否为空
- **FIFO 顺序**：使用 `VecDeque` 保证公平性
- **死亡标记**：`dead` 标志用于资源销毁时的清理
- **自旋锁保护**：使用 `SpinLock` 而非 `Mutex`，避免递归依赖

## 3. 核心 API：wait_until 家族

### 3.1 wait_until：核心等待原语

`wait_until` 是整个等待队列机制的核心，所有其他等待方法都基于它实现：

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

**关键设计思想**：
1. **先入队，再检查**：确保在检查条件和睡眠之间，任何唤醒都能被正确处理
2. **返回资源而非布尔值**：`Option<R>` 而非 `bool`，允许直接返回获取的资源（如锁 Guard）
3. **循环重试**：被唤醒后可能因竞争失败，需要重新入队并检查
4. **唯一 Waiter/Waker**：整个等待过程只创建一对对象，简化生命周期管理

### 3.2 wait_until 变体

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

**返回值语义**：
- `Ok(R)`：条件满足，返回获取的资源
- `Err(ERESTARTSYS)`：被信号中断
- `Err(EAGAIN_OR_EWOULDBLOCK)`：超时

### 3.3 为什么 wait_until 优于 wait_event

传统的 `wait_event` 系列 API 只返回布尔值：

```rust
// 传统方式（存在竞态）
wait_event(|| lock.is_available());
let guard = lock.acquire();  // ❌ 可能失败！另一个线程可能抢先获取
```

而 `wait_until` 可以原子地"等待并获取"：

```rust
// wait_until 方式（无竞态）
let guard = wait_until(|| lock.try_acquire());  // ✅ 原子获取
```

**关键优势**：
- 消除"检查-获取"之间的竞态窗口
- 代码更简洁，语义更清晰
- 性能更好（减少一次原子操作）

## 4. 等待与唤醒流程

### 4.1 详细等待流程

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

**关键点解释**：

1. **先入队再检查**：
   - 如果先检查再入队，可能在检查返回 false 之后、入队之前，条件变为 true 并发生唤醒
   - 此时唤醒信号会丢失，因为 waker 还未入队
   - 先入队保证了任何后续的唤醒都能找到我们的 waker

2. **循环重试的必要性**：
   - 被唤醒并不保证条件满足（可能是伪唤醒）
   - 即使条件曾经满足，也可能因为竞争而再次变为不满足
   - 因此需要重新入队并检查

3. **一次性 Waker 的优势**：
   - 避免了复杂的状态管理
   - 被唤醒后不会再次被唤醒（`has_woken` 标志）
   - 循环中会重新注册新的 waker，但使用同一个 `Arc<Waker>`

### 4.2 唤醒流程

#### wake_one：唤醒一个等待者

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

**设计要点**：
- **FIFO 顺序**：从队列头部取出，保证公平性
- **锁外唤醒**：释放锁后再调用 `waker.wake()`，减少锁竞争
- **自动跳过失效 waker**：如果目标进程已退出，自动尝试下一个

#### wake_all：唤醒所有等待者

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

**设计要点**：
- **批量取出**：一次性将队列清空，最小化锁持有时间
- **锁外唤醒**：在释放锁后逐个唤醒，允许被唤醒的进程立即竞争
- **返回实际唤醒数**：区分"唤醒请求"和"实际唤醒"

### 4.3 Waker 唤醒机制

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

**关键特性**：
- **原子唤醒标志**：`has_woken` 使用 `AtomicBool` 确保只唤醒一次
- **弱引用目标进程**：使用 `Weak<PCB>` 避免循环引用，进程退出时自动清理
- **内存序保证**：Release/Acquire 语义确保唤醒前的修改对被唤醒进程可见

### 4.4 阻塞当前进程

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

**关键设计**：
- **双重检查**：在禁用中断前后都检查唤醒标志，防止"先唤后睡"
- **中断保护**：`mark_sleep` 必须在禁用中断的情况下执行
- **信号检查**：被调度回来后检查是否有信号需要处理

## 5. 内存序与正确性

### 5.1 内存序保证

等待队列使用以下内存序确保正确性：

| 操作 | 内存序 | 作用 |
|------|--------|------|
| `waker.wake()` | Release | 确保唤醒前的修改对被唤醒者可见 |
| `waiter.wait()` | Acquire | 确保能看到唤醒前的所有修改 |
| `register_waker` | Release | 确保入队操作的可见性 |
| `num_waiters` | Acquire/Release | 同步计数器的修改 |

### 5.2 happens-before 关系

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

**保证**：
- 唤醒方在调用 `wake()` 之前的所有修改，对等待方可见
- 这是实现正确同步的基础

### 5.3 无竞态的证明

**情况 1：先入队后唤醒（正常流程）**
```
时间轴：
T1: 等待方 register_waker()
T2: 等待方检查条件，返回 false
T3: 等待方准备睡眠
T4: 唤醒方 wake_one()          ← waker 在队列中，正常唤醒
T5: 等待方被唤醒
```

**情况 2：先唤醒后入队（需要处理的竞态）**
```
时间轴：
T1: 等待方检查条件，返回 false
T2: 唤醒方修改条件为 true
T3: 唤醒方 wake_one()          ← waker 还未入队，唤醒失败
T4: 等待方 register_waker()
T5: 等待方再次检查条件         ← 检测到 true，不会睡眠！
```

**关键点**：通过"入队后再检查"，即使唤醒发生在入队前，也能通过第二次检查发现条件已满足。

## 6. 兼容 API：wait_event 家族

为了向后兼容，提供了基于 `wait_until` 实现的 `wait_event` 系列 API：

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

**before_sleep 钩子**：
- 在入队后、睡眠前执行
- 典型用途：释放锁，避免持锁睡眠
- 例如：`wait_event_interruptible(|| cond(), Some(|| drop(guard)))`

## 7. 便利方法

### 7.1 睡眠并释放锁

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

**使用示例**：
```rust
let guard = lock.lock();
// 检查条件
if !condition_met() {
    // 需要等待，释放锁并睡眠
    wait_queue.sleep_unlock_spinlock(guard)?;
    // 被唤醒后需要重新获取锁
}
```

### 7.2 队列生命周期管理

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

## 8. 事件等待队列

除了普通等待队列，还提供了基于事件掩码的等待队列：

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

**使用场景**：
- 多种事件类型的等待（如 `READABLE | WRITABLE`）
- 按事件类型唤醒特定等待者
- 例如：socket 的 poll/select 实现

## 9. 超时支持

### 9.1 超时机制

```rust
pub fn wait_until_timeout<F, R>(&self, cond: F, timeout: Duration)
    -> Result<R, SystemError>
where
    F: FnMut() -> Option<R>,
{
    self.wait_until_impl(cond, true, Some(timeout), None::<fn()>)
}
```

**实现原理**：
1. 计算截止时间：`deadline = now + timeout`
2. 创建定时器，到期时唤醒 waiter
3. 被唤醒后检查是否超时：
   - 如果定时器触发，返回 `EAGAIN_OR_EWOULDBLOCK`
   - 如果条件满足，取消定时器并返回结果

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

**关键设计**：
- 定时器通过 `Waker::wake()` 唤醒，而非直接唤醒 PCB
- 这样 `Waiter::wait()` 可以正确观察到唤醒状态
- 与正常唤醒使用相同的机制，保持一致性

## 10. 使用示例

### 10.1 信号量实现

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

**优势**：
- 使用 `wait_until` 确保原子地"等待并获取"
- 避免了"检查-获取"之间的竞态窗口
- 代码简洁清晰

### 10.2 条件变量实现

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

### 10.3 RwSem 集成

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

**为什么这样设计**：
- `try_read()` 返回 `Option<Guard>`，正好匹配 `wait_until` 的要求
- 一行代码实现完整的"等待并获取"逻辑
- 编译器保证类型安全，不会出现忘记检查的问题

### 10.4 带超时的等待

```rust
fn wait_with_timeout(queue: &WaitQueue, timeout_ms: u64) -> Result<(), SystemError> {
    let timeout = Duration::from_millis(timeout_ms);
    queue.wait_event_interruptible_timeout(
        || condition_met(),
        Some(timeout),
    )
}
```

## 11. 性能特性

### 11.1 快速路径优化

- **无等待者时**：`is_empty()` 仅需一次原子读取，无锁竞争
- **快速检查**：`wait_until` 首先检查条件，避免不必要的入队
- **锁外唤醒**：唤醒操作在释放队列锁之后进行，减少锁持有时间

### 11.2 可扩展性

- **FIFO 队列**：保证公平性，避免饥饿
- **批量唤醒**：`wake_all()` 一次性取出所有 waker，最小化锁竞争
- **跨 CPU 唤醒**：Waker 可以在不同 CPU 上唤醒，无需锁同步

### 11.3 内存开销

- **Waiter**：栈上分配，不涉及堆
- **Waker**：通过 `Arc` 共享，每个等待者一个
- **队列**：只在有等待者时占用空间，空队列开销最小

## 12. 对比与演进

### 12.1 与传统实现的对比

| 特性 | 传统 wait_event | DragonOS wait_until |
|------|----------------|---------------------|
| API 返回值 | `bool` | `Option<R>` |
| 原子获取 | 不支持（需手动） | **原生支持** |
| 唤醒丢失 | 需小心处理 | **设计保证无丢失** |
| 竞态窗口 | 检查-获取有窗口 | **无窗口** |
| 代码复杂度 | 高（需手动管理状态） | **低（编译器保证）** |
| 性能 | 可能需要多次原子操作 | **最小化原子操作** |

### 12.2 设计演进

**旧设计（存在问题）**：
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

**当前设计（正确）**：
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

## 13. 最佳实践

### 13.1 使用建议

1. **优先使用 wait_until**：相比 `wait_event`，它提供更强的保证和更简洁的代码
2. **利用 Option 返回值**：直接返回获取的资源，避免二次获取
3. **合理使用可中断版本**：用户态进程应使用 `*_interruptible` 避免无法终止
4. **正确处理超时**：区分 `ERESTARTSYS`（信号）和 `EAGAIN_OR_EWOULDBLOCK`（超时）

### 13.2 避免的陷阱

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

### 13.3 调试建议

- 使用 `wait_queue.len()` 检查等待者数量
- 注意 `has_woken` 标志的状态（通过日志）
- 检查是否有进程长期阻塞（可能是唤醒逻辑错误）

## 14. 实现原理总结

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

这个设计通过"先入队后检查"的核心机制、一次性 Waiter/Waker 模式和原子内存序保证，实现了一个零唤醒丢失、高性能、易用的等待队列机制，为 DragonOS 的各种同步原语提供了坚实的基础。
