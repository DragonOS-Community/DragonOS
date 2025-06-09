:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/locking/spinlock.md

- Translation time: 2025-05-19 01:43:03

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

(_translated_label___spinlock_doc_en)=

:::{note}
Author: Longjin <longjin@RinGoTek.cn>
:::

# Spinlock

## 1. Introduction

&emsp;&emsp;A spinlock is a type of lock used for synchronization in multi-threaded environments. Threads repeatedly check if the lock variable is available. Since the thread remains in a running state during this process, it is a form of busy waiting. Once a spinlock is acquired, the thread will hold onto it until it is explicitly released.

&emsp;&emsp;DragonOS implements spinlocks in the `kernel/src/lib/spinlock.rs` file. Based on slight differences in functional characteristics, two types of spinlocks, `RawSpinLock` and `SpinLock`, are provided.

(_translated_label___spinlock_doc_rawspinlock_en)=
## 2. RawSpinLock - Raw Spinlock

&emsp;&emsp;`RawSpinLock` is a raw spinlock, whose data part contains an AtomicBool, implementing the basic functionality of a spinlock. Its locking and unlocking require manual determination of the corresponding timing, meaning that, like spinlocks used in other languages, you need to first call the `lock()` method, and then manually call the `unlock()` method when leaving the critical section. We do not explicitly inform the compiler of which data the spinlock is protecting.

&emsp;&emsp;RawSpinLock provides programmers with very flexible control over locking and unlocking. However, due to its excessive flexibility, it is easy to make mistakes when using it. Common issues include "accessing critical section data without locking", "forgetting to unlock", and "double unlocking". The compiler cannot check for these issues, and they can only be discovered at runtime.

:::{warning}
`RawSpinLock` is not binary compatible with the C version of `spinlock_t`. If you need to operate on the C version of `spinlock_t` for temporary compatibility reasons, please use the operation functions for the C version of spinlock_t provided in `spinlock.rs`.

However, for newly developed features, please do not use the C version of `spinlock_t`, as it will be removed as code refactoring progresses.
:::

(_translated_label___spinlock_doc_spinlock_en)=
## 3. SpinLock - Spinlock with Guard

&emsp;&emsp;`SpinLock` is an encapsulation of `RawSpinLock`, enabling compile-time checks for issues such as "accessing critical section data without locking", "forgetting to unlock", and "double unlocking"; it also supports internal mutability of data.

&emsp;&emsp;Its struct prototype is as follows:

```rust
#[derive(Debug)]
pub struct SpinLock<T> {
    lock: RawSpinlock,
    /// 自旋锁保护的数据
    data: UnsafeCell<T>,
}
```

### 3.1. Usage

&emsp;&emsp;You can initialize a SpinLock like this:

```rust
let x = SpinLock::new(Vec::new());
```

&emsp;&emsp;When initializing this SpinLock, you must pass the data you want to protect into the SpinLock, which will then manage it.

&emsp;&emsp;When you need to read or modify data protected by SpinLock, please first use the `lock()` method of SpinLock. This method will return a `SpinLockGuard`. You can use the member functions of the protected data to perform some operations, or directly read and write the protected data. (This is equivalent to obtaining a mutable reference to the protected data.)

&emsp;&emsp;The complete example is shown in the code below:

```rust
let x :SpinLock<Vec<i32>>= SpinLock::new(Vec::new());
    {
        let mut g :SpinLockGuard<Vec<i32>>= x.lock();
        g.push(1);
        g.push(2);
        g.push(2);
        assert!(g.as_slice() == [1, 2, 2] || g.as_slice() == [2, 2, 1]);
        // 在此处，SpinLock是加锁的状态
        debug!("x={:?}", x);
    }
    // 由于上方的变量`g`，也就是SpinLock守卫的生命周期结束，自动释放了SpinLock。因此，在此处，SpinLock是放锁的状态
    debug!("x={:?}", x);
```

&emsp;&emsp;For variables inside a struct, we can use SpinLock to perform fine-grained locking, that is, wrap the member variables that need to be locked in SpinLock, for example:

```rust
pub struct a {
  pub data: SpinLock<data_struct>,
}
```

&emsp;&emsp;Of course, we can also lock the entire struct:

```rust
struct MyStruct {
  pub data: data_struct,
}
/// 被全局加锁的结构体
pub struct LockedMyStruct(SpinLock<MyStruct>);
```

### 3.2. Principle

&emsp;&emsp;`SpinLock` can achieve compile-time checking because it introduces a `SpinLockGuard` as a guard. When writing code, we ensure that only after calling the `lock()` method of `SpinLock` to acquire the lock can a `SpinLockGuard` be generated. Moreover, whenever we want to access protected data, we must obtain a guard. We also implement the `Drop` trait for `SpinLockGuard`; when the guard's lifetime ends, the lock will be automatically released. There is no other way to release the lock. Therefore, we can know that, in a context, as long as the `SpinLockGuard`'s lifetime has not ended, it has the right to access the critical section data, and the data access is safe.

### 3.3. Existing Issues

#### 3.3.1. Double Locking

&emsp;&emsp;Please note that the compile-time checks supported by `SpinLock` are not omnipotent. It currently cannot detect the issue of "double locking" at compile time. Consider this scenario: function A acquires the lock, and then function B attempts to lock again, which results in a "double locking" issue. This kind of problem cannot be detected at compile time.

&emsp;&emsp;To address this issue, we recommend the following programming approach:

- If function B needs to access data within the critical section, function B should receive a parameter of type `&SpinLockGuard`, which is obtained by function A. In this way, function B can access the data within the critical section.
