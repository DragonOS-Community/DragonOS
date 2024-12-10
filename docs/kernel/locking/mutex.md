(_mutex_doc)=

:::{note}
作者：龙进 <longjin@RinGoTek.cn>
:::

# mutex互斥量

&emsp;&emsp;mutex是一种轻量级的同步原语，只有被加锁、空闲两种状态。

&emsp;&emsp;当mutex被占用时，尝试对mutex进行加锁操作的进程将会被休眠，直到资源可用。

## 1. 特性

- 同一时间只有1个任务可以持有mutex
- 不允许递归地加锁、解锁
- 只允许通过mutex的api来操作mutex
- 在硬中断、软中断中不能使用mutex

## 2. 定义

&emsp;&emsp;mutex定义在`lib/mutex.rs`中，定义如下所示：

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

## 3. 使用

&emsp;&emsp;与SpinLock类似，Rust版本的Mutex具有一个守卫。使用的时候，需要将要被保护的数据的所有权移交Mutex。并且，守卫只能在加锁成功后产生，因此，每个时刻，每个Mutex最多存在1个守卫。

&emsp;&emsp;当需要读取、修改Mutex保护的数据时，请先使用Mutex的`lock()`方法。该方法会返回一个`MutexGuard`。您可以使用被保护的数据的成员函数来进行一些操作。或者是直接读取、写入被保护的数据。（相当于您获得了被保护的数据的可变引用）

&emsp;&emsp;完整示例如下方代码所示：

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

&emsp;&emsp;对于结构体内部的变量，我们可以使用Mutex进行细粒度的加锁，也就是使用Mutex包裹需要细致加锁的成员变量，比如这样：

```rust
pub struct a {
  pub data: Mutex<data_struct>,
}
```

&emsp;&emsp;当然，我们也可以对整个结构体进行加锁：

```rust
struct MyStruct {
  pub data: data_struct,
}
/// 被全局加锁的结构体
pub struct LockedMyStruct(Mutex<MyStruct>);
```

## 4. API

### 4.1. new - 初始化Mutex

#### 原型

```rust
pub const fn new(value: T) -> Self
```

#### 说明

&emsp;&emsp;`new()`方法用于初始化一个Mutex。该方法需要一个被保护的数据作为参数。并且，该方法会返回一个Mutex。


### 4.2. lock - 加锁

#### 原型

```rust
pub fn lock(&self) -> MutexGuard<T>
```

#### 说明

&emsp;&emsp;对Mutex加锁，返回Mutex的守卫，您可以使用这个守卫来操作被保护的数据。

&emsp;&emsp;如果Mutex已经被加锁，那么，该方法会阻塞当前进程，直到Mutex被释放。

### 4.3. try_lock - 尝试加锁

#### 原型

```rust
pub fn try_lock(&self) -> Result<MutexGuard<T>, i32>
```

#### 说明

&emsp;&emsp;尝试对Mutex加锁。如果加锁失败，不会将当前进程加入等待队列。如果加锁成功，返回Mutex的守卫；如果当前Mutex已经被加锁，返回`Err(错误码)`。

## 5. C版本的Mutex（在将来会被废弃）

&emsp;&emsp;mutex定义在`common/mutex.h`中。其数据类型如下所示：

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

&emsp;&emsp;初始化一个mutex对象。

#### mutex_lock

**`void mutex_lock(mutex_t *lock)`**

&emsp;&emsp;对一个mutex对象加锁。若mutex当前被其他进程持有，则当前进程进入休眠状态。

#### mutex_unlock

**`void mutex_unlock(mutex_t *lock)`**

&emsp;&emsp;对一个mutex对象解锁。若mutex的等待队列中有其他的进程，则唤醒下一个进程。

#### mutex_trylock

**`void mutex_trylock(mutex_t *lock)`**

&emsp;&emsp;尝试对一个mutex对象加锁。若mutex当前被其他进程持有，则返回0.否则，加锁成功，返回1.

#### mutex_is_locked

**`void mutex_is_locked(mutex_t *lock)`**

&emsp;&emsp;判断mutex是否已被加锁。若给定的mutex已处于上锁状态，则返回1，否则返回0。
