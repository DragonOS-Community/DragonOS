:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/locking/rwsem.md

- Translation time: 2026-01-01 07:41:09

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

### 2.1 Writer-Preferred Strategy

RwSem adopts a **writer-preferred** fairness strategy to prevent writers from being starved by continuous readers:

- When there are waiting writers, **block new readers** from acquiring the lock
- When the last reader releases the lock, prioritize waking up waiting writers
- Only wake up waiting readers after all writers have completed

### 2.2 State Bit Design

RwSem uses a 64-bit atomic integer (`AtomicU64`) to maintain all states:

```
+--------+------------------+-----------+--------+
| 63..33 |      32          |    31     | 30..0  |
+--------+------------------+-----------+--------+
|   ?    | WRITER_BIT       |  保留     | 读者计数 |
| 写者   | 写者激活标志     |           | (32位)  |
| 等待数 |                 |           |         |
+--------+------------------+-----------+--------+
```

| Field | Position | Description |
|-------|----------|-------------|
| `READER_MASK` | Bits 0..31 | Number of currently active readers |
| `WRITER_BIT` | Bit 32 | Whether a writer is currently holding the lock |
| `WRITER_WAITER_MASK` | Bits 33..63 | Number of waiting writers |

### 2.3 Fast Path and Slow Path

The acquisition operation of RwSem is divided into two phases:

#### Fast Path
- Attempts to acquire the lock using an atomic CAS (Compare-And-Swap) operation
- No locking required, no system call overhead
- Returns Guard directly upon success

#### Slow Path
- Entered when the fast path fails
- Uses `WaitQueue` to add the current thread to the wait queue and sleep
- Retries the fast path upon being awakened

### 2.4 Dual Wait Queue Design

RwSem maintains two separate wait queues:

```rust
pub struct RwSem<T> {
    wq_read:  WaitQueue,  // 读者等待队列
    wq_write: WaitQueue,  // 写者等待队列
}
```

**Design Rationale**:
- When a writer releases, only one writer is awakened (`wq_write.wakeup()`)
- When a writer releases and no writers are waiting, **all** readers are awakened (`wq_read.wakeup_all()`)
- Separate queues avoid unnecessary thread awakenings

## 3. Lock Acquisition Process

### 3.1 Read Lock Acquisition

```
┌─────────────────┐
│ 尝试快速路径     │
│ (CAS递增读者数)  │
└────────┬────────┘
         │
    成功 / 失败
      /     \
     ↓       ↓
  ┌───┐   ┌─────────────┐
  │返回│   │无写者激活/等待?│
  └───┘   └──────┬──────┘
                 │ 是/否
            是 /     \
           ↓         ↓
      ┌───────┐  ┌─────────┐
      │加入读者│  │阻塞并等待│
      │等待队列│  └────┬────┘
      └───────┘       │
                   被唤醒时重试
```

**Key Point**: Checks `WRITER_BIT` and `WRITER_WAITER_MASK`; if either is non-zero, the read lock cannot be acquired.

### 3.2 Write Lock Acquisition

```
┌─────────────────┐
│ 尝试快速路径     │
│ (CAS设置写者位)  │
└────────┬────────┘
         │
    成功 / 失败
      /     \
     ↓       ↓
  ┌───┐   ┌─────────────────────┐
  │返回│   │原子递增写者等待计数  │
  └───┘   └──────────┬──────────┘
                     │
                     ↓
              ┌───────────────┐
              │ 无读者/写者?   │
              └───────┬───────┘
                      │ 是/否
                 是 /     \
                ↓         ↓
            ┌───────┐  ┌─────────┐
            │成功： │  │阻塞并等待│
            │设置   │  └────┬────┘
            │写者位 │       │
            └───┬───┘   被唤醒时重试
                │
                ↓
            ┌─────┐
            │返回 │
            └─────┘
```

**Key Point**: Uses `WRITER_WAITER_UNIT` to preemptively "reserve" the lock, preventing new readers from entering.

## 4. Lock Release and Wakeup Strategy

### 4.1 Read Lock Release

```rust
fn read_unlock(&self) {
    // 原子递减读者计数
    let prev = self.count.fetch_sub(1, Ordering::Release);
    let current = prev - 1;

    // 最后一个读者 + 有写者等待 → 唤醒一个写者
    if (current & READER_MASK) == 0 && (current & WRITER_WAITER_MASK) > 0 {
        self.wq_write.wakeup(None);
    }
}
```

### 4.2 Write Lock Release

```rust
fn write_unlock(&self) {
    // 清除写者位
    let prev = self.count.fetch_and(!WRITER_BIT, Ordering::Release);

    if 有写者等待 {
        唤醒一个写者
    } else {
        唤醒所有读者
    }
}
```

**Wakeup Strategy**:
1. Writers waiting → Wake up one writer (maintaining writer preference)
2. No writers waiting → Wake up all readers (concurrent reading)

## 5. Main APIs

### 5.1 Creation

```rust
pub const fn new(value: T) -> Self
```

### 5.2 Read Lock Acquisition

```rust
// 阻塞获取 (不可中断)
pub fn read(&self) -> RwSemReadGuard<'_, T>

// 阻塞获取 (可被信号中断)
pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_read(&self) -> Option<RwSemReadGuard<'_, T>
```

### 5.3 Write Lock Acquisition

```rust
// 阻塞获取 (不可中断)
pub fn write(&self) -> RwSemWriteGuard<'_, T>

// 阻塞获取 (可被信号中断)
pub fn write_interruptible(&self) -> Result<RwSemWriteGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_write(&self) -> Option<RwSemWriteGuard<'_, T>
```

### 5.4 Write Lock Downgrade

```rust
impl<'a, T> RwSemWriteGuard<'a, T> {
    /// 将写锁降级为读锁，不释放锁
    pub fn downgrade(self) -> RwSemReadGuard<'a, T>
}
```

**Downgrade Operation**: Atomically transitions the state from "1 writer" to "1 reader" and wakes up other readers when no writers are waiting.

## 6. Usage Example

```rust
use kernel::libs::rwsem::RwSem;

static DATA: RwSem<Vec<u32>> = RwSem::new(Vec::new());

// 读取数据
fn reader() {
    let guard = DATA.read();
    println!("Data: {:?}", *guard);
    // guard 离开作用域时自动释放读锁
}

// 写入数据
fn writer() {
    let mut guard = DATA.write();
    guard.push(42);
    // guard 离开作用域时自动释放写锁
}

// 写锁降级
fn writer_with_downgrade() {
    let mut guard = DATA.write();
    guard.clear();
    guard.push(1);

    // 降级为读锁，允许其他读者并发访问
    let read_guard = guard.downgrade();
    println!("After write: {:?}", *read_guard);
}

// 可中断的获取
fn interruptible_reader() -> Result<(), SystemError> {
    let guard = DATA.read_interruptible()?;
    println!("Data: {:?}", *guard);
    Ok(())
}

// 非阻塞尝试
fn try_read() -> Option<()> {
    let guard = DATA.try_read()?;
    println!("Data: {:?}", *guard);
    Some(())
}
```

## 7. Comparison with Linux Kernel rw_semaphore

| Feature | Linux rw_semaphore | DragonOS RwSem |
|---------|-------------------|----------------|
| State Storage | atomic_long_t | AtomicU64 |
| Writer Preference | Yes | Yes |
| Dual Wait Queues | Yes | Yes |
| Lock Downgrade | Supported | Supported |
| Interruptible Wait | Supported | Supported |

## 8. Notes

1. **Not for interrupt context** - RwSem may sleep, which is not allowed in interrupt context
2. **Avoid nested locks** - Recursive acquisition of the same RwSem by the same thread can cause deadlocks
3. **Writer preference may starve readers** - In writer-intensive scenarios, readers may experience prolonged waits
4. **Memory ordering requirements** - Use Acquire/Release semantics to ensure happens-before relationships
