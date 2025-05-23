:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/locking/mutex.md

- Translation time: 2025-05-19 01:41:16

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

(_translated_label___mutex_doc_en)=

:::{note}
Author: Longjin <longjin@RinGoTek.cn>
:::

# Mutex (Mutual Exclusion)

&emsp;&emsp;A mutex is a lightweight synchronization primitive, with only two states: locked and idle.

&emsp;&emsp;When a mutex is occupied, any process attempting to lock it will be put to sleep until the resource becomes available.

## 1. Features

- Only one task can hold the mutex at a time.
- Recursive locking and unlocking are not allowed.
- Mutex can only be operated through its API.
- Mutex cannot be used in hard interrupts or soft interrupts.

## 2. Definition

&emsp;&emsp;The mutex is defined in `lib/mutex.rs`, as shown below:

```rust
/// @brief Mutex互斥量结构体
/// 请注意！由于Mutex属于休眠锁，因此，如果您的代码可能在中断上下文内执行，请勿采用Mutex！
#[derive(Debug)]
pub struct Mutex<T> {
    /// 该Mutex保护的数据
    data: UnsafeCell<T>,
    /// Mutex内部的信息
    inner: SpinLock<MutexInner>,
}

#[derive(Debug)]
struct MutexInner {
    /// 当前Mutex是否已经被上锁(上锁时，为true)
    is_locked: bool,
    /// 等待获得这个锁的进程的链表
    wait_list: LinkedList<&'static mut process_control_block>,
}

```

## 3. Usage

&emsp;&emsp;Similar to SpinLock, the Rust version of Mutex has a guard. When using it, you need to transfer the ownership of the data to be protected to the Mutex. Moreover, the guard can only be generated after a successful lock, so at any moment, each Mutex can have at most one guard.

&emsp;&emsp;When you need to read or modify the data protected by the Mutex, you should first use the `lock()` method of the Mutex. This method returns a `MutexGuard`. You can use the member functions of the protected data to perform some operations, or directly read or write the protected data. (This is equivalent to obtaining a mutable reference to the protected data.)

&emsp;&emsp;A complete example is shown in the code below:

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

&emsp;&emsp;For variables inside a structure, we can use Mutex to perform fine-grained locking, that is, wrap the member variables that need to be locked in detail with Mutex, for example:

```rust
pub struct a {
  pub data: Mutex<data_struct>,
}
```

&emsp;&emsp;Of course, we can also lock the entire structure:

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

&emsp;&emsp;The `new()` method is used to initialize a Mutex. This method requires a protected data as a parameter. It returns a Mutex.

### 4.2. lock - Lock

#### Prototype

```rust
pub fn lock(&self) -> MutexGuard<T>
```

#### Description

&emsp;&emsp;Lock the Mutex, returns the guard of the Mutex. You can use this guard to operate the protected data.

&emsp;&emsp;If the Mutex is already locked, this method will block the current process until the Mutex is released.

### 4.3. try_lock - Try to Lock

#### Prototype

```rust
pub fn try_lock(&self) -> Result<MutexGuard<T>, i32>
```

#### Description

&emsp;&emsp;Try to lock the Mutex. If the lock fails, the current process will not be added to the waiting queue. If the lock is successful, it returns the guard of the Mutex; if the Mutex is already locked, it returns `Err(错误码)`.

## 5. C Version of Mutex (Will be deprecated in the future)

&emsp;&emsp;The mutex is defined in `common/mutex.h`. Its data type is as follows:

```c
typedef struct
{

    atomic_t count; // 锁计数。1->已解锁。 0->已上锁,且有可能存在等待者
    spinlock_t wait_lock;   // mutex操作锁，用于对mutex的list的操作进行加锁
    struct List wait_list;  // Mutex的等待队列
} mutex_t;
```

### 5.1. API

#### mutex_init

**`void mutex_init(mutex_t *lock)`**

&emsp;&emsp;Initialize a mutex object.

#### mutex_lock

**`void mutex_lock(mutex_t *lock)`**

&emsp;&emsp;Lock a mutex object. If the mutex is currently held by another process, the current process will enter a sleep state.

#### mutex_unlock

**`void mutex_unlock(mutex_t *lock)`**

&emsp;&emsp;Unlock a mutex object. If there are other processes in the mutex's waiting queue, the next process will be awakened.

#### mutex_trylock

**`void mutex_trylock(mutex_t *lock)`**

&emsp;&emsp;Try to lock a mutex object. If the mutex is currently held by another process, it returns 0. Otherwise, the lock is successful and returns 1.

#### mutex_is_locked

**`void mutex_is_locked(mutex_t *lock)`**

&emsp;&emsp;Determine if the mutex is already locked. If the given mutex is in a locked state, it returns 1; otherwise, it returns 0.
