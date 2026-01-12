# RwSem 读写信号量

## 1. 简介

RwSem (Read-Write Semaphore) 是一种可睡眠的读写锁，用于保护进程上下文中的共享数据。与自旋锁实现的 RwLock 不同，RwSem 在无法获取锁时会**让出 CPU 并进入睡眠状态**，而不是忙等待。

### 1.1 适用场景

| 特性 | RwLock (spinlock) | RwSem (semaphore) |
|------|-------------------|-------------------|
| 上下文 | 进程 / 中断 | 仅进程上下文 |
| 等待方式 | 忙等待 (自旋) | 睡眠 (调度) |
| 适用场景 | 短时间临界区 | 长时间临界区 |
| 中断上下文 | 可用 | **不可用** |

**重要**: RwSem **不能在中断上下文中使用**，因为它可能会睡眠。

## 2. 核心设计

### 2.1 锁状态表示

RwSem 使用一个原子整数 (`AtomicUsize`) 维护锁状态，通过位域编码实现高效的状态管理：

```
64位系统状态位布局:
+--------+--------------+------------+----------+------------------+
| Bit 63 |   Bit 62     |  Bit 61    | Bit 60   |   Bits 59..0     |
+--------+--------------+------------+----------+------------------+
| WRITER | UPGRADEABLE  |  BEING     | OVERFLOW |  READER COUNT    |
|        |   READER     | UPGRADED   | DETECT   |   (读者计数)      |
+--------+--------------+------------+----------+------------------+
```

| 字段 | 位置 | 说明 |
|------|------|------|
| `WRITER` | Bit 63 | 写者锁，1 表示有写者持有锁 |
| `UPGRADEABLE_READER` | Bit 62 | 可升级读者锁，1 表示有可升级读者持有锁 |
| `BEING_UPGRADED` | Bit 61 | 升级进行中标志，1 表示正在从可升级读者升级到写者 |
| `MAX_READER` | Bit 60 | 读者溢出检测位，当读者数量达到 2^60 时置位 |
| `READER_COUNT` | Bits 59..0 | 当前激活的读者数量 |

```rust
const READER: usize = 1;
const WRITER: usize = 1 << (usize::BITS - 1);              // Bit 63
const UPGRADEABLE_READER: usize = 1 << (usize::BITS - 2);  // Bit 62
const BEING_UPGRADED: usize = 1 << (usize::BITS - 3);      // Bit 61
const MAX_READER: usize = 1 << (usize::BITS - 4);          // Bit 60
```

### 2.2 三种锁模式

RwSem 支持三种锁模式，提供不同级别的访问权限：

1. **读锁（Read Lock）**
   - 多个读者可以并发持有读锁
   - 提供对数据的只读访问（`&T`）
   - 与写者和正在升级的可升级读者互斥

2. **写锁（Write Lock）**
   - 只有一个写者可以持有写锁
   - 提供对数据的可变访问（`&mut T`）
   - 与所有其他锁模式互斥

3. **可升级读锁（Upgradeable Read Lock）**
   - 只有一个可升级读者可以持有可升级读锁
   - 初始提供只读访问（`&T`）
   - 可以原子地升级到写锁
   - 与写者和其他可升级读者互斥，但可与普通读者共存

### 2.3 等待队列设计

```rust
pub struct RwSem<T: ?Sized> {
    lock: AtomicUsize,      // 锁状态
    queue: WaitQueue,       // 单一等待队列
    val: UnsafeCell<T>,     // 被保护的数据
}
```

**设计特点**：
- 使用单一 `WaitQueue` 管理所有等待者（读者、写者、可升级读者）
- 利用 `WaitQueue.wait_until()` 的原子语义确保正确性
- 通过不同的唤醒策略实现公平性和性能平衡

## 3. 锁获取机制

### 3.1 读锁获取

```rust
pub fn read(&self) -> RwSemReadGuard<'_, T> {
    self.queue.wait_until(|| self.try_read())
}

pub fn try_read(&self) -> Option<RwSemReadGuard<'_, T>> {
    let lock = self.lock.fetch_add(READER, Acquire);
    if lock & (WRITER | BEING_UPGRADED | MAX_READER) == 0 {
        // 无写者、无升级中的可升级读者、未溢出 → 成功
        Some(RwSemReadGuard {
            inner: self,
            _nosend: PhantomData,
        })
    } else {
        // 有阻塞因素，回滚计数
        self.lock.fetch_sub(READER, Release);
        None
    }
}
```

**获取条件**：
- 无写者（`WRITER` 位为 0）
- 无正在升级的可升级读者（`BEING_UPGRADED` 位为 0）
- 读者数量未溢出（`MAX_READER` 位为 0）

**关键设计**：
- 先乐观地递增读者计数（`fetch_add`）
- 再检查阻塞条件
- 失败则回滚计数（`fetch_sub`）
- 这种"先递增后检查"的方式在无竞争场景下性能更优

### 3.2 写锁获取

```rust
pub fn write(&self) -> RwSemWriteGuard<'_, T> {
    self.queue.wait_until(|| self.try_write())
}

pub fn try_write(&self) -> Option<RwSemWriteGuard<'_, T>> {
    if self.lock.compare_exchange(0, WRITER, Acquire, Relaxed).is_ok() {
        Some(RwSemWriteGuard {
            inner: self,
            _nosend: PhantomData,
        })
    } else {
        None
    }
}
```

**获取条件**：
- 锁状态必须为 0（无读者、无写者、无可升级读者）
- 使用 CAS 操作保证原子性

### 3.3 可升级读锁获取

```rust
pub fn upread(&self) -> RwSemUpgradeableGuard<'_, T> {
    self.queue.wait_until(|| self.try_upread())
}

pub fn try_upread(&self) -> Option<RwSemUpgradeableGuard<'_, T>> {
    let lock = self.lock.fetch_or(UPGRADEABLE_READER, Acquire)
                 & (WRITER | UPGRADEABLE_READER);
    if lock == 0 {
        // 无写者且无其他可升级读者 → 成功
        return Some(RwSemUpgradeableGuard {
            inner: self,
            _nosend: PhantomData,
        });
    } else if lock == WRITER {
        // 有写者，需要回滚
        self.lock.fetch_sub(UPGRADEABLE_READER, Release);
    }
    // lock == UPGRADEABLE_READER 表示已有其他可升级读者，
    // fetch_or 没有改变状态，无需回滚
    None
}
```

**获取条件**：
- 无写者（`WRITER` 位为 0）
- 无其他可升级读者（`UPGRADEABLE_READER` 位为 0）
- 可与普通读者共存

**关键设计**：
- 使用 `fetch_or` 原子设置可升级读者位
- 通过检查返回的旧值判断是否成功
- 只在设置了 `WRITER` 位但失败时才需要回滚

### 3.4 wait_until 核心机制

所有阻塞式的锁获取都依赖 `WaitQueue.wait_until()` 方法：

```rust
pub fn wait_until<F, R>(&self, cond: F) -> R
where
    F: FnMut() -> Option<R>,
{
    // 1. 快速路径：先检查条件
    if let Some(res) = cond() {
        return res;
    }

    // 2. 创建一对 Waiter/Waker
    let (waiter, waker) = Waiter::new_pair();

    loop {
        // 3. 先注册 waker 到等待队列
        self.register_waker(waker.clone());

        // 4. 再检查条件（防止唤醒丢失）
        if let Some(res) = cond() {
            self.remove_waker(&waker);
            return res;
        }

        // 5. 睡眠等待被唤醒
        waiter.wait();

        // 6. 被唤醒后循环继续（可能是伪唤醒）
    }
}
```

**关键点**：
- 先注册 waker，再检查条件，确保不会错过任何唤醒信号
- 即使被唤醒，也可能获取锁失败（公平竞争），需要继续循环
- 这种设计避免了复杂的唤醒丢失问题

## 4. 锁释放与唤醒策略

### 4.1 读锁释放

```rust
impl<T: ?Sized> Drop for RwSemReadGuard<'_, T> {
    fn drop(&mut self) {
        // 原子递减读者计数
        if self.inner.lock.fetch_sub(READER, Release) == READER {
            // 这是最后一个读者，唤醒一个等待者
            self.inner.queue.wake_one();
        }
    }
}
```

**唤醒策略**：
- 只有最后一个读者释放时才唤醒
- 唤醒一个等待者（可能是写者或可升级读者）
- 避免不必要的唤醒开销

### 4.2 写锁释放

```rust
impl<T: ?Sized> Drop for RwSemWriteGuard<'_, T> {
    fn drop(&mut self) {
        // 清除写者位
        self.inner.lock.fetch_and(!WRITER, Release);

        // 唤醒所有等待者
        self.inner.queue.wake_all();
    }
}
```

**唤醒策略**：
- 唤醒所有等待者
- 允许多个读者并发获取锁
- 只有一个写者能成功（通过 CAS 竞争）

### 4.3 可升级读锁释放

```rust
impl<T: ?Sized> Drop for RwSemUpgradeableGuard<'_, T> {
    fn drop(&mut self) {
        let res = self.inner.lock.fetch_sub(UPGRADEABLE_READER, Release);
        if res == UPGRADEABLE_READER {
            // 没有其他读者，唤醒所有等待者
            self.inner.queue.wake_all();
        }
    }
}
```

**唤醒策略**：
- 如果没有其他读者（`res == UPGRADEABLE_READER`），唤醒所有等待者
- 如果还有其他读者，不唤醒（等待最后一个读者唤醒）

## 5. 高级特性

### 5.1 写锁降级

将写锁原子地降级为可升级读锁，允许其他读者并发访问：

```rust
impl<'a, T> RwSemWriteGuard<'a, T> {
    pub fn downgrade(mut self) -> RwSemUpgradeableGuard<'a, T> {
        loop {
            self = match self.try_downgrade() {
                Ok(guard) => return guard,
                Err(e) => e,
            };
        }
    }

    fn try_downgrade(self) -> Result<RwSemUpgradeableGuard<'a, T>, Self> {
        let inner = self.inner;
        let res = self.inner.lock.compare_exchange(
            WRITER,
            UPGRADEABLE_READER,
            AcqRel,
            Relaxed,
        );
        if res.is_ok() {
            core::mem::forget(self);
            Ok(RwSemUpgradeableGuard {
                inner,
                _nosend: PhantomData,
            })
        } else {
            Err(self)
        }
    }
}
```

**使用场景**：
- 写入数据后，需要长时间持有锁进行只读操作
- 通过降级允许其他读者并发访问，提高并发性

**注意事项**：
- 降级过程中使用 CAS 操作保证原子性
- 使用循环重试处理 CAS 失败（通常是由于 ABA 问题）
- 降级后不会唤醒等待者（由可升级读锁释放时处理）

### 5.2 可升级读锁升级

将可升级读锁原子地升级为写锁：

```rust
impl<'a, T> RwSemUpgradeableGuard<'a, T> {
    pub fn upgrade(mut self) -> RwSemWriteGuard<'a, T> {
        // 先设置升级标志，阻塞新的读者
        self.inner.lock.fetch_or(BEING_UPGRADED, Acquire);
        loop {
            self = match self.try_upgrade() {
                Ok(guard) => return guard,
                Err(e) => e,
            };
        }
    }

    pub fn try_upgrade(self) -> Result<RwSemWriteGuard<'a, T>, Self> {
        let res = self.inner.lock.compare_exchange(
            UPGRADEABLE_READER | BEING_UPGRADED,
            WRITER | UPGRADEABLE_READER,
            AcqRel,
            Relaxed,
        );
        if res.is_ok() {
            let inner = self.inner;
            core::mem::forget(self);
            Ok(RwSemWriteGuard {
                inner,
                _nosend: PhantomData,
            })
        } else {
            Err(self)
        }
    }
}
```

**升级机制**：
1. 先设置 `BEING_UPGRADED` 标志，阻塞新的读者
2. 自旋等待现有读者全部释放（读者计数降为 0）
3. 使用 CAS 将状态从"可升级读者+升级中"转换为"写者"

**关键设计**：
- 升级过程中不会睡眠，而是自旋等待
- `BEING_UPGRADED` 标志确保不会有新的读者进入
- 保持 `UPGRADEABLE_READER` 位直到升级完成，防止其他线程获取可升级读锁

### 5.3 可中断的锁获取

支持被信号中断的锁获取操作：

```rust
// 可中断的读锁获取
pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError> {
    self.queue.wait_until_interruptible(|| self.try_read())
}

// 可中断的写锁获取
pub fn write_interruptible(&self) -> Result<RwSemWriteGuard<'_, T>, SystemError> {
    self.queue.wait_until_interruptible(|| self.try_write())
}
```

**使用场景**：
- 用户态进程获取锁时，需要响应信号（如 Ctrl+C）
- 避免进程无限期阻塞

**错误处理**：
- 返回 `Err(SystemError::ERESTARTSYS)` 表示被信号中断
- 调用者需要适当处理错误（通常是返回到用户态）

## 6. API 参考

### 6.1 创建

```rust
// 编译期常量初始化
pub const fn new(value: T) -> Self

// 运行时初始化
let rwsem = RwSem::new(data);
```

### 6.2 读锁操作

```rust
// 阻塞获取（不可中断）
pub fn read(&self) -> RwSemReadGuard<'_, T>

// 阻塞获取（可被信号中断）
pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_read(&self) -> Option<RwSemReadGuard<'_, T>>
```

### 6.3 写锁操作

```rust
// 阻塞获取（不可中断）
pub fn write(&self) -> RwSemWriteGuard<'_, T>

// 阻塞获取（可被信号中断）
pub fn write_interruptible(&self) -> Result<RwSemWriteGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_write(&self) -> Option<RwSemWriteGuard<'_, T>>
```

### 6.4 可升级读锁操作

```rust
// 阻塞获取（不可中断）
pub fn upread(&self) -> RwSemUpgradeableGuard<'_, T>

// 非阻塞尝试获取
pub fn try_upread(&self) -> Option<RwSemUpgradeableGuard<'_, T>>
```

### 6.5 锁转换操作

```rust
impl<'a, T> RwSemWriteGuard<'a, T> {
    // 写锁降级为可升级读锁
    pub fn downgrade(self) -> RwSemUpgradeableGuard<'a, T>
}

impl<'a, T> RwSemUpgradeableGuard<'a, T> {
    // 可升级读锁升级为写锁
    pub fn upgrade(self) -> RwSemWriteGuard<'a, T>

    // 非阻塞尝试升级
    pub fn try_upgrade(self) -> Result<RwSemWriteGuard<'a, T>, Self>
}
```

### 6.6 直接访问

```rust
// 获取可变引用（需要独占的 &mut self）
pub fn get_mut(&mut self) -> &mut T
```

## 7. 使用示例

### 7.1 基本读写

```rust
use crate::libs::rwsem::RwSem;

static DATA: RwSem<Vec<u32>> = RwSem::new(Vec::new());

// 多个读者可并发访问
fn reader() {
    let guard = DATA.read();
    println!("Data: {:?}", *guard);
    // guard 离开作用域时自动释放读锁
}

// 写者独占访问
fn writer() {
    let mut guard = DATA.write();
    guard.push(42);
    // guard 离开作用域时自动释放写锁
}
```

### 7.2 可升级读锁

```rust
fn reader_that_may_write() {
    // 先以可升级读者身份获取锁
    let guard = DATA.upread();

    // 读取数据判断是否需要修改
    if guard.is_empty() {
        // 需要修改，升级到写锁
        let mut write_guard = guard.upgrade();
        write_guard.push(1);
    }
    // 不需要修改，直接释放
}
```

### 7.3 写锁降级

```rust
fn writer_with_downgrade() {
    // 先获取写锁进行修改
    let mut guard = DATA.write();
    guard.clear();
    guard.push(1);

    // 修改完成，降级为可升级读锁
    let read_guard = guard.downgrade();

    // 现在允许其他读者并发访问
    println!("After write: {:?}", *read_guard);
}
```

### 7.4 可中断的获取

```rust
fn interruptible_reader() -> Result<(), SystemError> {
    // 可被信号中断
    let guard = DATA.read_interruptible()?;
    println!("Data: {:?}", *guard);
    Ok(())
}
```

### 7.5 非阻塞尝试

```rust
fn try_reader() -> Option<()> {
    // 立即返回，不会睡眠
    let guard = DATA.try_read()?;
    println!("Data: {:?}", *guard);
    Some(())
}
```

## 8. 内存序与正确性

### 8.1 内存序保证

RwSem 使用以下内存序保证正确性：

- **Acquire**：获取锁时使用，确保锁保护的数据可见
- **Release**：释放锁时使用，确保临界区内的修改对后续获取者可见
- **AcqRel**：同时需要 Acquire 和 Release 语义的操作（如降级、升级）
- **Relaxed**：CAS 失败路径，不需要同步

### 8.2 happens-before 关系

```
写者释放 (Release) ────┐
                      │ happens-before
读者获取 (Acquire) ←──┘
```

**保证**：
- 写者在释放前的所有修改，对后续读者可见
- 多个读者之间没有 happens-before 关系（并发读）

## 9. 与其他实现的对比

| 特性 | Linux rw_semaphore | Rust parking_lot::RwLock | DragonOS RwSem |
|------|-------------------|--------------------------|----------------|
| 状态存储 | atomic_long_t | AtomicUsize | AtomicUsize |
| 等待队列 | 单队列 + 类型标记 | 单队列 + parking | 单队列 + WaitQueue |
| 可升级锁 | 不支持 | 支持 | **支持** |
| 锁降级 | 支持（down_write_to_read） | 支持 | **支持** |
| 公平策略 | 写者优先 + HANDOFF | FIFO + 反饥饿 | FIFO 公平竞争 |
| 可中断等待 | 支持 | 不支持 | **支持** |
| 中断上下文 | 不支持 | 不支持 | 不支持 |

## 10. 性能特性

### 10.1 快速路径优化

- **无竞争读取**：单次原子操作（`fetch_add`）
- **无竞争写入**：单次 CAS 操作
- **读者释放**：单次原子操作，只有最后一个读者才唤醒

### 10.2 可扩展性

- **并发读**：读者之间完全并发，无额外同步开销
- **写者唤醒**：`wake_all()` 允许多个读者并发唤醒
- **队列开销**：只在有等待者时才操作队列

### 10.3 性能建议

- 优先使用 `try_*` 方法避免睡眠（在能够快速重试的场景下）
- 使用可升级读锁避免读锁升级导致的死锁
- 读密集场景下性能优于写密集场景

## 11. 注意事项

### 11.1 使用限制

1. **不可在中断上下文使用** - RwSem 可能会睡眠
2. **避免嵌套锁** - 同一线程递归获取同一 RwSem 会导致死锁
3. **避免读锁升级** - 不支持将普通读锁升级为写锁（会导致死锁）
4. **Guard 不可跨线程** - Guard 类型标记为 `!Send`，不能跨线程传递

### 11.2 死锁场景

```rust
// ❌ 错误：嵌套获取同一锁
let guard1 = rwsem.read();
let guard2 = rwsem.write();  // 死锁！

// ❌ 错误：尝试升级普通读锁
let guard = rwsem.read();
drop(guard);
let guard = rwsem.write();  // 不是原子升级，可能有竞态

// ✅ 正确：使用可升级读锁
let guard = rwsem.upread();
let guard = guard.upgrade();  // 原子升级
```

### 11.3 最佳实践

1. 优先使用可升级读锁而非普通读锁（当可能需要写入时）
2. 使用降级而非释放+重新获取（保持原子性）
3. 在用户态进程中使用 `*_interruptible` 变体
4. 尽量减少临界区大小，避免长时间持有锁

## 12. 实现原理总结

```
                    ┌─────────────────────────────────────┐
                    │             RwSem<T>                │
                    │                                     │
                    │  ┌───────────────────────────────┐  │
                    │  │  lock: AtomicUsize            │  │
                    │  │  [W|U|B|O|READER_COUNT]       │  │
                    │  │  W: Writer                    │  │
                    │  │  U: Upgradeable Reader        │  │
                    │  │  B: Being Upgraded            │  │
                    │  │  O: Overflow Detect           │  │
                    │  └───────────────────────────────┘  │
                    │  ┌───────────────────────────────┐  │
                    │  │  queue: WaitQueue             │  │
                    │  │  (all waiters in FIFO order)  │  │
                    │  └───────────────────────────────┘  │
                    │  ┌───────────────────────────────┐  │
                    │  │  val: UnsafeCell<T>           │  │
                    │  └───────────────────────────────┘  │
                    └─────────────────────────────────────┘

锁获取流程（以读锁为例）:
    read() → wait_until(try_read)
                  │
                  ↓
           ┌──────────────┐
           │ 注册 waker    │ ← 先入队（防止唤醒丢失）
           └──────┬───────┘
                  │
                  ↓
           ┌──────────────┐
           │ 调用 cond()  │ ← 再检查条件
           │ = try_read() │
           └──────┬───────┘
                  │
            成功 ←┴─→ 失败
              │         │
              ↓         ↓
         返回 Guard   睡眠等待
                         │
                    被唤醒后循环

锁释放与唤醒:
    读锁: 最后一个读者 → wake_one()
    写锁: 总是 → wake_all()
    可升级读锁: 无其他读者时 → wake_all()
```

这个设计通过巧妙利用位域编码、wait_until 原子等待机制和差异化的唤醒策略，实现了一个高效、正确且功能丰富的读写信号量。
