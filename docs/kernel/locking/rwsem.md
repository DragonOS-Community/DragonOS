# RwSem读写信号量

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

### 2.1 写者优先策略

RwSem 采用**写者优先**的公平性策略，目的是防止写者被连续的读者饿死：

- 当有写者在等待时，**阻塞新的读者**获取锁
- 最后一个读者释放锁时，优先唤醒等待的写者
- 所有写者完成后，才唤醒等待的读者

### 2.2 状态位设计

RwSem 使用一个 64 位原子整数 (`AtomicU64`) 维护所有状态：

```
+--------+------------------+-----------+--------+
| 63..33 |      32          |    31     | 30..0  |
+--------+------------------+-----------+--------+
|   ?    | WRITER_BIT       |  保留     | 读者计数 |
| 写者   | 写者激活标志     |           | (32位)  |
| 等待数 |                 |           |         |
+--------+------------------+-----------+--------+
```

| 字段 | 位置 | 说明 |
|------|------|------|
| `READER_MASK` | Bits 0..31 | 当前激活的读者数量 |
| `WRITER_BIT` | Bit 32 | 是否有写者正在持有锁 |
| `WRITER_WAITER_MASK` | Bits 33..63 | 等待中的写者数量 |

### 2.3 快速路径与慢速路径

RwSem 的获取操作分为两个阶段：

#### 快速路径
- 使用原子 CAS (Compare-And-Swap) 操作尝试获取锁
- 无需加锁，无系统调用开销
- 成功时直接返回 Guard

#### 慢速路径
- 快速路径失败时进入
- 使用 `WaitQueue` 将当前线程加入等待队列并睡眠
- 被唤醒时重试快速路径

### 2.4 双等待队列设计

RwSem 维护两个独立的等待队列：

```rust
pub struct RwSem<T> {
    wq_read:  WaitQueue,  // 读者等待队列
    wq_write: WaitQueue,  // 写者等待队列
}
```

**设计原因**：
- 写者释放时，只唤醒一个写者（`wq_write.wakeup()`）
- 写者释放且无写者等待时，唤醒**所有**读者（`wq_read.wakeup_all()`）
- 分离队列可避免不必要的线程唤醒

## 3. 锁获取流程

### 3.1 读锁获取

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

**关键点**: 检查 `WRITER_BIT` 和 `WRITER_WAITER_MASK`，任一非零则不能获取读锁。

### 3.2 写锁获取

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

**关键点**: 通过 `WRITER_WAITER_UNIT` 预先"占位"，阻止新读者进入。

## 4. 锁释放与唤醒策略

### 4.1 读锁释放

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

### 4.2 写锁释放

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

**唤醒策略**:
1. 有写者等待 → 唤醒一个写者（保持写者优先）
2. 无写者等待 → 唤醒所有读者（并发读）

## 5. 主要 API

### 5.1 创建

```rust
pub const fn new(value: T) -> Self
```

### 5.2 读锁获取

```rust
// 阻塞获取 (不可中断)
pub fn read(&self) -> RwSemReadGuard<'_, T>

// 阻塞获取 (可被信号中断)
pub fn read_interruptible(&self) -> Result<RwSemReadGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_read(&self) -> Option<RwSemReadGuard<'_, T>
```

### 5.3 写锁获取

```rust
// 阻塞获取 (不可中断)
pub fn write(&self) -> RwSemWriteGuard<'_, T>

// 阻塞获取 (可被信号中断)
pub fn write_interruptible(&self) -> Result<RwSemWriteGuard<'_, T>, SystemError>

// 非阻塞尝试获取
pub fn try_write(&self) -> Option<RwSemWriteGuard<'_, T>
```

### 5.4 写锁降级

```rust
impl<'a, T> RwSemWriteGuard<'a, T> {
    /// 将写锁降级为读锁，不释放锁
    pub fn downgrade(self) -> RwSemReadGuard<'a, T>
}
```

**降级操作**：原子地将状态从 "1个写者" 转换为 "1个读者"，并在无写者等待时唤醒其他读者。

## 6. 用法示例

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

## 7. 与 Linux 内核 rw_semaphore 的对比

| 特性 | Linux rw_semaphore | DragonOS RwSem |
|------|-------------------|----------------|
| 状态存储 | atomic_long_t | AtomicU64 |
| 写者优先 | 是 | 是 |
| 双等待队列 | 是 | 是 |
| 锁降级 | 支持 | 支持 |
| 可中断等待 | 支持 | 支持 |

## 8. 注意事项

1. **不可在中断上下文使用** - RwSem 可能会睡眠，中断上下文不允许睡眠
2. **避免嵌套锁** - 同一线程递归获取同一 RwSem 会导致死锁
3. **写者优先可能导致读者饿死** - 在写者密集的场景下，读者可能长时间等待
4. **内存序要求** - 使用 Acquire/Release 语义确保 happens-before 关系
