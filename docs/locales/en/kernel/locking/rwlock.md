:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/locking/rwlock.md

- Translation time: 2025-05-19 01:41:57

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# RwLock Read-Write Lock
:::{note}
Author: sujintao

Email: <sujintao@dragonos.org>
:::

## 1. Introduction
&emsp;&emsp;A read-write lock is a mechanism used in a concurrent environment to protect shared data among multiple processes. Compared to a regular spinlock, a read-write lock divides access to shared data into two types: read access and write access. Read access to shared data is controlled by a read lock, while write access to shared data is controlled by a write lock. The design of a read-write lock allows for multiple "readers" (read-only access) and a single "writer" (write access) to coexist simultaneously. For shared data that is mostly read-only, using a read-write lock to control access can improve performance to some extent.

## 2. Implementation of Read-Write Lock in DragonOS
### 2.1 Mechanism of Read-Write Lock
&emsp;&emsp;The purpose of a read-write lock is to maintain the consistency of shared variables in a multi-threaded system. Data is wrapped in an RwLock data structure, and all access and modification must be done through this structure. Each process that accesses shared data will obtain a guard. A read-only process obtains a READER (reader guard), while a process that needs to modify a shared variable obtains a WRITER (writer guard). As a "shadow" of the RwLock, threads perform access and modification operations based on the guard.

&emsp;&emsp;In practice, in addition to READER and WRITER, a read-write lock also introduces an UPGRADER. This is a guard that lies between READER and WRITER. The role of the UPGRADER is to prevent WRITER starvation. When a process obtains an UPGRADER, it treats it as a READER. However, the UPGRADER can be upgraded, and after upgrading, it becomes a WRITER guard, allowing write operations on shared data.

&emsp;&emsp;All guards satisfy the RAII mechanism native to Rust. When the scope of a guard ends, the guard will automatically release.

### 2.2 Relationship Between Read-Write Lock Guards
&emsp;&emsp;At any given time, multiple READERS can exist, meaning that multiple processes can access shared data simultaneously. However, only one WRITER can exist at a time, and when a process obtains a WRITER, no READERS or UPGRADERS can exist. A process can obtain an UPGRADER only if there are no existing UPGRADERS or WRITERS. However, once a process obtains an UPGRADER, it cannot successfully apply for a READER.

### 2.3 Design Details

#### 2.3.1 RwLock Data Structure
```rust
pub struct RwLock<T> {
    lock: AtomicU32,//原子变量
    data: UnsafeCell<T>,
}
```

#### 2.3.2 READER Guard Data Structure
```rust
pub struct RwLockReadGuard<'a, T: 'a> {
    data: *const T,
    lock: &'a AtomicU32,
}
```

#### 2.3.3 UPGRADER Guard Data Structure
```rust
pub struct RwLockUpgradableGuard<'a, T: 'a> {
    data: *const T,
    inner: &'a RwLock<T>,
}
```

#### 2.3.4 WRITER Guard Data Structure
```rust
pub struct RwLockWriteGuard<'a, T: 'a> {
    data: *mut T,
    inner: &'a RwLock<T>,
}
```

#### 2.3.5 Introduction to the lock Structure in RwLock
The lock is a 32-bit atomic variable AtomicU32, and its bit allocation is as follows:
```
                                                       UPGRADER_BIT     WRITER_BIT
                                                         ^                   ^
OVERFLOW_BIT                                             +------+    +-------+
  ^                                                             |    |
  |                                                             |    |
+-+--+--------------------------------------------------------+-+--+-+--+
|    |                                                        |    |    |
|    |                                                        |    |    |
|    |             The number of the readers                  |    |    |
|    |                                                        |    |    |
+----+--------------------------------------------------------+----+----+
  31  30                                                    2   1    0
```

&emsp;&emsp;(From right to left) The 0th bit represents whether WRITER is valid. If WRITER_BIT = 1, it indicates that a process has obtained a WRITER guard. If UPGRADER_BIT = 1, it indicates that a process has obtained an UPGRADER guard. Bits 2 to 30 are used to represent the number of processes that have obtained READER guards in binary form. The 31st bit is an overflow detection bit. If OVERFLOW_BIT = 1, new requests for obtaining READER guards will be rejected.

## 3. Main APIs of Read-Write Lock
### 3.1 Main APIs of RwLock
```rust
///功能:  输入需要保护的数据类型data,返回一个新的RwLock类型.
pub const fn new(data: T) -> Self
```
```rust
///功能: 获得READER守卫
pub fn read(&self) -> RwLockReadGuard<T>
```
```rust
///功能: 尝试获得READER守卫
pub fn try_read(&self) -> Option<RwLockReadGuard<T>>
```
```rust
///功能: 获得WRITER守卫
pub fn write(&self) -> RwLockWriteGuard<T>
```
```rust
///功能: 尝试获得WRITER守卫
pub fn try_write(&self) -> Option<RwLockWriteGuard<T>>
```
```rust
///功能: 获得UPGRADER守卫
pub fn upgradeable_read(&self) -> RwLockUpgradableGuard<T>
```
```rust
///功能: 尝试获得UPGRADER守卫
pub fn try_upgradeable_read(&self) -> Option<RwLockUpgradableGuard<T>>
```
### 3.2 Main APIs of the WRITER Guard RwLockWriteGuard
```rust
///功能: 将WRITER降级为READER
pub fn downgrade(self) -> RwLockReadGuard<'rwlock, T>
```
```rust
///功能: 将WRITER降级为UPGRADER
pub fn downgrade_to_upgradeable(self) -> RwLockUpgradableGuard<'rwlock, T>
```
### 3.3 Main APIs of the UPGRADER Guard RwLockUpgradableGuard
```rust
///功能: 将UPGRADER升级为WRITER
pub fn upgrade(mut self) -> RwLockWriteGuard<'rwlock, T> 
```
```rust
///功能: 将UPGRADER降级为READER
pub fn downgrade(self) -> RwLockReadGuard<'rwlock, T>
```

## 4. Usage Examples
```rust
static LOCK: RwLock<u32> = RwLock::new(100 as u32);

fn t_read1() {
    let guard = LOCK.read();
    let value = *guard;
    let readers_current = LOCK.reader_count();
    let writers_current = LOCK.writer_count();
    println!(
        "Reader1: the value is {value}
    There are totally {writers_current} writers, {readers_current} readers"
    );
}

fn t_read2() {
    let guard = LOCK.read();
    let value = *guard;
    let readers_current = LOCK.reader_count();
    let writers_current = LOCK.writer_count();
    println!(
        "Reader2: the value is {value}
    There are totally {writers_current} writers, {readers_current} readers"
    );
}

fn t_write() {
    let mut guard = LOCK.write();
    *guard += 100;
    let writers_current = LOCK.writer_count();
    let readers_current = LOCK.reader_count();
    println!(
        "Writers: the value is {guard}
    There are totally {writers_current} writers, {readers_current} readers",
        guard = *guard
    );
    let read_guard=guard.downgrade();
    let value=*read_guard;
    println!("After downgraded to read_guard: {value}");
}

fn t_upgrade() {
    let guard = LOCK.upgradeable_read();
    let value = *guard;
    let readers_current = LOCK.reader_count();
    let writers_current = LOCK.writer_count();
    println!(
        "Upgrader1 before upgrade: the value is {value}
    There are totally {writers_current} writers, {readers_current} readers"
    );
    let mut upgraded_guard = guard.upgrade();
    *upgraded_guard += 100;
    let writers_current = LOCK.writer_count();
    let readers_current = LOCK.reader_count();
    println!(
        "Upgrader1 after upgrade: the value is {temp}
    There are totally {writers_current} writers, {readers_current} readers",
        temp = *upgraded_guard
    );
    let downgraded_guard=upgraded_guard.downgrade_to_upgradeable();
    let value=*downgraded_guard;
    println!("value after downgraded: {value}");
    let read_guard=downgraded_guard.downgrade();
    let value_=*read_guard;
    println!("value after downgraded to read_guard: {value_}");
}

fn main() {
    let r2=thread::spawn(t_read2);
    let r1 = thread::spawn(t_read1);
    let t1 = thread::spawn(t_write);
    let g1 = thread::spawn(t_upgrade);
    r1.join().expect("r1");
    t1.join().expect("t1");
    g1.join().expect("g1");
    r2.join().expect("r2");
}
```
