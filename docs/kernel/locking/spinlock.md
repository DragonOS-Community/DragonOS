(_spinlock_doc)=

:::{note}
作者：龙进 <longjin@RinGoTek.cn>
:::

# 自旋锁

## 1.简介

&emsp;&emsp;自旋锁是用于多线程同步的一种锁，线程反复检查锁变量是否可用。由于线程在这一过程中保持运行的状态，因此是一种忙等待。一旦获取了自旋锁，线程会一直保持该锁，直至显式释放自旋锁。

&emsp;&emsp;DragonOS在`kernel/src/lib/spinlock.rs`文件中，实现了自旋锁。根据功能特性的略微差异，分别提供了`RawSpinLock`和`SpinLock`两种自旋锁。

(_spinlock_doc_rawspinlock)=
## 2. RawSpinLock - 原始自旋锁

&emsp;&emsp;`RawSpinLock`是原始的自旋锁，其数据部分包含一个AtomicBool, 实现了自旋锁的基本功能。其加锁、放锁需要手动确定对应的时机，也就是说，和我们在其他语言中使用的自旋锁一样，
需要先调用`lock()`方法，然后当离开临界区时，手动调用`unlock()`方法。我们并没有向编译器显式地指定该自旋锁到底保护的是哪些数据。

&emsp;&emsp;RawSpinLock为程序员提供了非常自由的加锁、放锁控制。但是，正是由于它过于自由，因此在使用它的时候，我们很容易出错。很容易出现“未加锁就访问临界区的数据”、“忘记放锁”、“双重释放”等问题。当使用RawSpinLock时，编译器并不能对这些情况进行检查，这些问题只能在运行时被发现。

:::{warning}
`RawSpinLock`与C版本的`spinlock_t`不具有二进制兼容性。如果由于暂时的兼容性的需求，要操作C版本的`spinlock_t`,请使用`spinlock.rs`中提供的C版本的spinlock_t的操作函数。

但是，对于新开发的功能，请不要使用C版本的`spinlock_t`，因为随着代码重构的进行，我们将会移除它。
:::

(_spinlock_doc_spinlock)=
## 3. SpinLock - 具备守卫的自旋锁

&emsp;&emsp;`SpinLock`在`RawSpinLock`的基础上，进行了封装，能够在编译期检查出“未加锁就访问临界区的数据”、“忘记放锁”、“双重释放”等问题；并且，支持数据的内部可变性。

&emsp;&emsp;其结构体原型如下：

```rust
#[derive(Debug)]
pub struct SpinLock<T> {
    lock: RawSpinlock,
    /// 自旋锁保护的数据
    data: UnsafeCell<T>,
}
```

### 3.1. 使用方法

&emsp;&emsp;您可以这样初始化一个SpinLock：

```rust
let x = SpinLock::new(Vec::new());
```

&emsp;&emsp;在初始化这个SpinLock时，必须把要保护的数据传入SpinLock，由SpinLock进行管理。

&emsp;&emsp;当需要读取、修改SpinLock保护的数据时，请先使用SpinLock的`lock()`方法。该方法会返回一个`SpinLockGuard`。您可以使用被保护的数据的成员函数来进行一些操作。或者是直接读取、写入被保护的数据。（相当于您获得了被保护的数据的可变引用）

&emsp;&emsp;完整示例如下方代码所示：

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

&emsp;&emsp;对于结构体内部的变量，我们可以使用SpinLock进行细粒度的加锁，也就是使用SpinLock包裹需要细致加锁的成员变量，比如这样：

```rust
pub struct a {
  pub data: SpinLock<data_struct>,
}
```

&emsp;&emsp;当然，我们也可以对整个结构体进行加锁：

```rust
struct MyStruct {
  pub data: data_struct,
}
/// 被全局加锁的结构体
pub struct LockedMyStruct(SpinLock<MyStruct>);
```

### 3.2. 原理

&emsp;&emsp;`SpinLock`之所以能够实现编译期检查，是因为它引入了一个`SpinLockGuard`作为守卫。我们在编写代码的时候，保证只有调用`SpinLock`的`lock()`方法加锁后，才能生成一个`SpinLockGuard`。 并且，当我们想要访问受保护的数据的时候，都必须获得一个守卫。然后，我们为`SpinLockGuard`实现了`Drop` trait，当守卫的生命周期结束时，将会自动释放锁。除此以外，没有别的方法能够释放锁。因此我们能够得知，一个上下文中，只要`SpinLockGuard`的生命周期没有结束，那么它就拥有临界区数据的访问权，数据访问就是安全的。

### 3.3. 存在的问题

#### 3.3.1. 双重加锁

&emsp;&emsp;请注意，`SpinLock`支持的编译期检查并不是万能的。它目前无法在编译期检查出“双重加锁”问题。试看这样一个场景：函数A中，获得了锁。然后函数B中继续尝试加锁，那么就造成了“双重加锁”问题。这样在编译期是无法检测出来的。

&emsp;&emsp;针对这个问题，我们建议采用这样的编程方法：

- 如果函数B需要访问临界区内的数据，那么，函数B应当接收一个类型为`&SpinLockGuard`的参数，这个守卫由函数A获得。这样一来，函数B就能访问临界区内的数据。
