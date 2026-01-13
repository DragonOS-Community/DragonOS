:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/locking/mutex.md

- Translation time: 2026-01-13 12:51:18

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

(_translated_label___mutex_doc_en)=

:::{note}
Author: Long Jin <longjin@RinGoTek.cn>
:::

# mutex (Mutual Exclusion Lock)

&emsp;&emsp;A mutex is a lightweight synchronization primitive that has only two states: locked or free.

&emsp;&emsp;When a mutex is held, any process attempting to lock it will be put to sleep until the resource becomes available.

## 1. Features

- Only one task can hold the mutex at any given time
- Does not allow recursive locking/unlocking
- Can only be manipulated through the mutex's API
- Cannot be used in hard interrupts or soft interrupts

## 2. Definition

&emsp;&emsp;The mutex is defined in `lib/mutex.rs`, as shown below:

```rust
/// @brief Mutex互斥量结构体
/// 请注意！由于Mutex属于休眠锁，因此，如果您的代码可能在中断上下文内执行，请勿采用Mutex！
#[derive(Debug)]
pub struct Mutex<T> {
    /// 该Mutex保护的数据
    data: UnsafeCell<T>,
    /// Mutex锁状态
    lock: AtomicBool,
    /// 等待队列（Waiter/Waker 机制避免唤醒丢失）
    wait_queue: WaitQueue,
}
```

## 3. Usage

&emsp;&emsp;Similar to SpinLock, the Rust version of Mutex has a guard. When using it, you need to transfer ownership of the data to be protected to the Mutex. Moreover, the guard can only be created after a successful lock, so there can be at most one guard for each Mutex at any time.

&emsp;&emsp;When you need to read or modify the data protected by the Mutex, first use the `lock()` method of the Mutex. This method returns a `MutexGuard`. You can then use the member functions of the protected data to perform operations, or directly read/write the protected data (equivalent to obtaining a mutable reference to the protected data).

&emsp;&emsp;A complete example is shown in the following code:

```rust
let x :Mutex<Vec<i32>>= Mutex::new(Vec::new());
    {
        let mut g :MutexGuard<Vec<i32>>= x.lock();
        g.push(1);
        g.push(2);
        g.push(2);
        assert!(g.as_slice() == [1, 2, 2] || g.as_slice() == [2, 2, 1]);
        // 在此处，Mutex是加锁的状态
        debug!("x={:?}", x);
    }
    // 由于上方的变量`g`，也就是Mutex守卫的生命周期结束，自动释放了Mutex。因此，在此处，Mutex是放锁的状态
    debug!("x={:?}", x);
```

&emsp;&emsp;For variables inside a struct, we can use Mutex for fine-grained locking, i.e., wrapping the member variables that require detailed locking with Mutex, like this:

```rust
pub struct a {
  pub data: Mutex<data_struct>,
}
```

&emsp;&emsp;Of course, we can also lock the entire struct:

```rust
struct MyStruct {
  pub data: data_struct,
}
/// 被全局加锁的结构体
pub struct LockedMyStruct(Mutex<MyStruct>);
```

## 4. API

### 4.1. new - Initialize Mutex

#### Prototype

```rust
pub const fn new(value: T) -> Self
```

#### Description

&emsp;&emsp;The `new()` method is used to initialize a Mutex. This method takes the data to be protected as a parameter and returns a Mutex.

### 4.2. lock - Acquire Lock

#### Prototype

```rust
pub fn lock(&self) -> MutexGuard<T>
```

#### Description

&emsp;&emsp;Acquires the Mutex lock and returns the Mutex guard, which you can use to manipulate the protected data.

&emsp;&emsp;If the Mutex is already locked, this method will block and wait via `WaitQueue.wait_until()` until the lock becomes available. The waiting process uses a Waiter/Waker state machine handshake to prevent wake-up loss.

### 4.3. try_lock - Attempt to Acquire Lock

#### Prototype

```rust
pub fn try_lock(&self) -> Result<MutexGuard<T>, i32>
```

#### Description

&emsp;&emsp;Attempts to acquire the Mutex lock. If the attempt fails, the current process is not added to the waiting queue. If the lock is successfully acquired, the Mutex guard is returned; if the Mutex is already locked, `Err(错误码)` is returned.
