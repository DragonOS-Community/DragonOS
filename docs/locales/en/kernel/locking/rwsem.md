:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/locking/rwsem.md

- Translation time: 2026-01-13 06:32:36

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# RwSem Read-Write Semaphore

## 1. Introduction

RwSem (Read-Write Semaphore) is a sleepable read-write lock used to protect shared data in process context. Unlike the spinlock-based RwLock, RwSem will **yield the CPU and enter sleep state** when it fails to acquire the lock, rather than busy-waiting.

### 1.1 Applicable Scenarios

| Feature | RwLock (spinlock) | RwSem (semaphore) |
|---------|-------------------|-------------------|
| Context | Process / Interrupt | Process context only |
| Waiting Method | Busy-waiting (spinning) | Sleeping (scheduling) |
| Applicable Scenario | Short critical sections | Long critical sections |
| Interrupt Context | Available | **Not available** |

**Important**: RwSem **cannot be used in interrupt context** as it may sleep.

## 2. Core Design

### 2.1 Lock State Representation

RwSem uses an atomic integer (`AtomicUsize`) to maintain the lock state, with efficient state management through bitfield encoding:

```
64位系统状态位布局:
+--------+--------------+------------+----------+------------------+
| Bit 63 |   Bit 62     |  Bit 61    | Bit 60   |   Bits 59..0     |
+--------+--------------+------------+----------+------------------+
| WRITER | UPGRADEABLE  |  BEING     | OVERFLOW |  READER COUNT    |
|        |   READER     | UPGRADED   | DETECT   |   (读者计数)      |
+--------+--------------+------------+----------+------------------+
```

| Field | Position | Description |
|-------|----------|-------------|
| `WRITER` | Bit 63 | Writer lock, 1 indicates a writer holds the lock |
| `UPGRADEABLE_READER` | Bit 62 | Upgradeable reader lock, 1 indicates an upgradeable reader holds the lock |
| `BEING_UPGRADED` | Bit 61 | Upgrade-in-progress flag, 1 indicates upgrading from upgradeable reader to writer |
| `MAX_READER` | Bit 60 | Reader overflow detection bit, set when reader count reaches 2^60 |
| `READER_COUNT` | Bits 59..0 | Current number of active readers |

```rust
const READER: usize = 1;
const WRITER: usize = 1 << (usize::BITS - 1);              // Bit 63
const UPGRADEABLE_READER: usize = 1 << (usize::BITS - 2);  // Bit 62
const BEING_UPGRADED: usize = 1 << (usize::BITS - 3);      // Bit 61
const MAX_READER: usize = 1 << (usize::BITS - 4);          // Bit 60
```

### 2.2 Three Lock Modes

RwSem supports three lock modes, providing different levels of access permissions:

1. **Read Lock**
   - Multiple readers can hold the read lock concurrently
   - Provides read-only access to data (`&T`)
   - Mutually exclusive with writers and upgrade-in-progress readers

2. **Write Lock**
   - Only one writer can hold the write lock
   - Provides mutable access to data (`&mut T`)
   - Mutually exclusive with all other lock modes

3. **Upgradeable Read Lock**
   - Only one upgradeable reader can hold this lock
   - Initially provides read-only access (`&T`)
   - Can be atomically upgraded to a write lock
   - Mutually exclusive with writers and other upgradeable readers, but can coexist with regular readers

### 2.3 Wait Queue Design

```rust
pub struct RwSem<T: ?Sized> {
    lock: AtomicUsize,      // 锁状态
    queue: WaitQueue,       // 单一等待队列
    val: UnsafeCell<T>,     // 被保护的数据
}
```

**Design Features**:
- Uses a single `WaitQueue` to manage all waiters (readers, writers, upgradeable readers)
- Leverages the atomic semantics of `WaitQueue.wait_until()` to ensure correctness
- Balances fairness and performance through different wake-up strategies

## 3. Lock Acquisition Mechanism

### 3.1 Read Lock Acquisition

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

**Acquisition Conditions**:
- No writer (`WRITER` bit is 0)
- No upgrade-in-progress reader (`BEING_UPGRADED` bit is 0)
- Reader count not overflowed (`MAX_READER` bit is 0)

**Key Design**:
- Optimistically increments reader count first (`fetch_add`)
- Then checks blocking conditions
- Rolls back count if failed (`fetch_sub`)
- This "increment then check" approach performs better in non-contention scenarios

### 3.2 Write Lock Acquisition

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

**Acquisition Conditions**:
- Lock state must be 0 (no readers, no writers, no upgradeable readers)
- Uses CAS operation to ensure atomicity

### 3.3 Upgradeable Read Lock Acquisition

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

**Acquisition Conditions**:
- No writer (`WRITER` bit is 0)
- No other upgradeable readers (`UPGRADEABLE_READER` bit is 0)
- Can coexist with regular readers

**Key Design**:
- Uses `fetch_or` to atomically set the upgradeable reader bit
- Determines success by checking the returned old value
- Only needs to roll back if `WRITER` bit was set but failed

### 3.4 wait_until Core Mechanism

All blocking lock acquisitions rely on the `WaitQueue.wait_until()` method:

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

**Key Points**:
- Registers waker before checking conditions to avoid missing wake signals
- May fail to acquire the lock even after being woken (fair competition), requiring continued looping
- This design avoids complex wake-loss issues

## 4. Lock Release and Wake-up Strategies

### 4.1 Read Lock Release

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

**Wake-up Strategy**:
- Only wakes when the last reader releases
- Wakes one waiter (may be a writer or upgradeable reader)
- Avoids unnecessary wake-up overhead

### 4.2 Write Lock Release

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

**Wake-up Strategy**:
- Wakes all waiters
- Allows multiple readers to acquire the lock concurrently
- Only one writer can succeed (through CAS competition)

### 4.3 Upgradeable Read Lock Release

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

**Wake-up Strategy**:
- If no other readers (`res == UPGRADEABLE_READER`), wakes all waiters
- If other readers remain, doesn't wake (waits for last reader to wake)

## 5. Advanced Features

### 5.1 Write Lock Downgrade

Atomically downgrades a write lock to an upgradeable read lock, allowing concurrent reader access:

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

**Use Cases**:
- After writing data, needing to hold the lock for prolonged read operations
- Downgrading allows concurrent reader access, improving concurrency

**Notes**:
- Uses CAS operations to ensure atomicity during downgrade
- Uses loop retries to handle CAS failures (typically due to ABA issues)
- Doesn't wake waiters after downgrade (handled during upgradeable read release)

### 5.2 Upgradeable Read Lock Upgrade

Atomically upgrades an upgradeable read lock to a write lock:

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

**Upgrade Mechanism**:
1. First sets `BEING_UPGRADED` flag to block new readers
2. Spins waiting for existing readers to release (reader count drops to 0)
3. Uses CAS to transition state from "upgradeable reader + upgrading" to "writer"

**Key Design**:
- Doesn't sleep during upgrade, instead spins
- `BEING_UPGRADED` flag ensures no new readers enter
- Maintains `UPGRADEABLE_READER` bit until upgrade completes to prevent other threads from acquiring upgradeable read locks

### 5.3 Interruptible Lock Acquisition

Supports lock acquisition operations that can be interrupted by signals:

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

**Use Cases**:
- User-space processes acquiring locks need to respond to signals (e.g., Ctrl+C)
- Prevents indefinite process blocking

**Error Handling**:
- Returns `Err(SystemError::ERESTARTSYS)` to indicate signal interruption
- Callers need to handle errors appropriately (typically returning to user-space)

## 6. API Reference

### 6.1 Creation

```rust
// 编译期常量初始化
pub const fn new(value: T) -> Self

// 运行时初始化
let rwsem = RwSem::new(data);
```

### 6.2 Read Lock Operations

```rust
// 阻塞获取（不可中断）
pub fn read(&self) -> RwSemReadGuard<'_, T>

// 阻塞获取（可被信号中断）
pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_read(&self) -> Option<RwSemReadGuard<'_, T>>
```

### 6.3 Write Lock Operations

```rust
// 阻塞获取（不可中断）
pub fn write(&self) -> RwSemWriteGuard<'_, T>

// 阻塞获取（可被信号中断）
pub fn write_interruptible(&self) -> Result<RwSemWriteGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_write(&self) -> Option<RwSemWriteGuard<'_, T>>
```

### 6.4 Upgradeable Read Lock Operations

```rust
// 阻塞获取（不可中断）
pub fn upread(&self) -> RwSemUpgradeableGuard<'_, T>

// 非阻塞尝试获取
pub fn try_upread(&self) -> Option<RwSemUpgradeableGuard<'_, T>>
```

### 6.5 Lock Conversion Operations

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

### 6.6 Direct Access

```rust
// 获取可变引用（需要独占的 &mut self）
pub fn get_mut(&mut self) -> &mut T
```

## 7. Usage Examples

### 7.1 Basic Read-Write

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

### 7.2 Upgradeable Read Lock

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

### 7.3 Write Lock Downgrade

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

### 7.4 Interruptible Acquisition

```rust
fn interruptible_reader() -> Result<(), SystemError> {
    // 可被信号中断
    let guard = DATA.read_interruptible()?;
    println!("Data: {:?}", *guard);
    Ok(())
}
```

### 7.5 Non-blocking Attempt

```rust
fn try_reader() -> Option<()> {
    // 立即返回，不会睡眠
    let guard = DATA.try_read()?;
    println!("Data: {:?}", *guard);
    Some(())
}
```

## 8. Memory Ordering and Correctness

### 8.1 Memory Ordering Guarantees

RwSem uses the following memory orderings to ensure correctness:

- **Acquire**: Used when acquiring locks, ensures visibility of protected data
- **Release**: Used when releasing locks, ensures modifications in critical sections are visible to subsequent acquirers
- **AcqRel**: Operations requiring both Acquire and Release semantics (e.g., downgrades, upgrades)
- **Relaxed**: CAS failure paths, no synchronization needed

### 8.2 Happens-Before Relationships

```
写者释放 (Release) ────┐
                      │ happens-before
读者获取 (Acquire) ←──┘
```

**Guarantees**:
- All modifications by a writer before release are visible to subsequent readers
- No happens-before relationships between multiple readers (concurrent reads)

## 9. Comparison with Other Implementations

| Feature | Linux rw_semaphore | Rust parking_lot::RwLock | DragonOS RwSem |
|---------|-------------------|--------------------------|----------------|
| State Storage | atomic_long_t | AtomicUsize | AtomicUsize |
| Wait Queue | Single queue + type tagging | Single queue + parking | Single queue + WaitQueue |
| Upgradeable Locks | Not supported | Supported | **Supported** |
| Lock Downgrade | Supported (down_write_to_read) | Supported | **Supported** |
| Fairness Policy | Writer priority + HANDOFF | FIFO + anti-starvation | FIFO fair competition |
| Interruptible Waits | Supported | Not supported | **Supported** |
| Interrupt Context | Not supported | Not supported | Not supported |

## 10. Performance Characteristics

### 10.1 Fast Path Optimizations

- **Uncontended Reads**: Single atomic operation (`fetch_add`)
- **Uncontended Writes**: Single CAS operation
- **Reader Release**: Single atomic operation, only last reader wakes

### 10.2 Scalability

- **Concurrent Reads**: Fully concurrent among readers, no additional synchronization overhead
- **Writer Wake-up**: `wake_all()` allows concurrent reader wake-ups
- **Queue Overhead**: Only operates on the queue when waiters exist

### 10.3 Performance Recommendations

- Prefer `try_*` methods to avoid sleeping (in scenarios where quick retries are possible)
- Use upgradeable read locks to avoid deadlocks from read-to-write upgrades
- Outperforms in read-heavy scenarios compared to write-heavy scenarios

## 11. Notes

### 11.1 Usage Restrictions

1. **Not for Interrupt Context** - RwSem may sleep
2. **Avoid Nested Locks** - Recursive acquisition by the same thread leads to deadlocks
3. **Avoid Read-to-Write Upgrades** - Doesn't support upgrading regular read locks to write locks (causes deadlocks)
4. **Guards Not Thread-Safe** - Guard types are marked as `!Send`, cannot be passed across threads

### 11.2 Deadlock Scenarios

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

### 11.3 Best Practices

1. Prefer upgradeable read locks over regular read locks when writes might be needed
2. Use downgrades instead of release-and-reacquire (maintains atomicity)
3. Use `*_interruptible` variants in user-space processes
4. Minimize critical section sizes to avoid prolonged lock holding

## 12. Implementation Summary

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

This design achieves an efficient, correct, and feature-rich read-write semaphore through clever use of bitfield encoding, the wait_until atomic waiting mechanism, and differentiated wake-up strategies.
