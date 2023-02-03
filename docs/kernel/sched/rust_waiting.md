# 与“等待”相关的api（rust语言）

&emsp;&emsp;如果几个进程需要等待某个事件发生，才能被运行，那么就需要一种“等待”的机制，以实现进程同步。

## 1. WaitQueue等待队列

&emsp;&emsp; WaitQueue是一种进程同步机制，中文名为“等待队列”。它可以将当前进程挂起，并在时机成熟时，由另一个进程唤醒他们。

&emsp;&emsp;当您需要等待一个事件完成时，使用 WaitQueue机制能减少进程同步的开销。相比于滥用自旋锁以及信号量，或者是循环使用usleep(1000)这样的函数来完成同步， WaitQueue是一个高效的解决方案。

### 1.1 WaitQueue的使用

&emsp;&emsp; WaitQueue的使用非常简单，只需要三步：

1. 初始化一个WaitQueue对象。
2. 调用这个WaitQueue的挂起相关的API，将当前进程挂起。
3. 当事件发生时，由另一个进程，调用这个WaitQueue的唤醒相关的API，唤醒一个进程。

&emsp;&emsp;下面是一个简单的例子：

### 1.1.1 初始化一个WaitQueue对象

&emsp;&emsp; WaitQueue对象的初始化非常简单，只需要调用WaitQueue::INIT即可。

```rust
let mut wq = WaitQueue::INIT;
```

### 1.1.2 挂起进程

&emsp;&emsp; 您可以这样挂起当前进程：

```rust
wq.sleep();
```

&emsp;&emsp; 当前进程会被挂起，直到有另一个进程调用了`wq.wakeup()`。

### 1.1.3 唤醒进程

&emsp;&emsp; 您可以这样唤醒一个进程：

```rust
// 唤醒等待队列头部的进程（如果它的state & PROC_INTERRUPTIBLE 不为0）
wq.wakeup(PROC_INTERRUPTIBLE);

// 唤醒等待队列头部的进程（如果它的state & PROC_UNINTERRUPTIBLE 不为0）
wq.wakeup(PROC_UNINTERRUPTIBLE);

// 唤醒等待队列头部的进程（无论它的state是什么）
wq.wakeup((-1) as u64);
```

### 1.2 API

### 1.2.1 挂起进程

&emsp;&emsp;您可以使用以下函数，将当前进程挂起，并插入到指定的等待队列。这些函数大体功能相同，只是在一些细节上有所不同。

| 函数名                                     | 解释                                                            |
| --------------------------------------- | ------------------------------------------------------------- |
| sleep()                                 | 将当前进程挂起，并设置进程状态为PROC_INTERRUPTIBLE                            |
| sleep_uninterruptible()                 | 将当前进程挂起，并设置进程状态为PROC_UNINTERRUPTIBLE                          |
| sleep_unlock_spinlock()                 | 将当前进程挂起，并设置进程状态为PROC_INTERRUPTIBLE。待当前进程被插入等待队列后，解锁给定的自旋锁     |
| sleep_unlock_mutex()                    | 将当前进程挂起，并设置进程状态为PROC_INTERRUPTIBLE。待当前进程被插入等待队列后，解锁给定的Mutex   |
| sleep_uninterruptible_unlock_spinlock() | 将当前进程挂起，并设置进程状态为PROC_UNINTERRUPTIBLE。待当前进程被插入等待队列后，解锁给定的自旋锁   |
| sleep_uninterruptible_unlock_mutex()    | 将当前进程挂起，并设置进程状态为PROC_UNINTERRUPTIBLE。待当前进程被插入等待队列后，解锁给定的Mutex |

### 1.2.2 唤醒进程

&emsp;&emsp;您可以使用`wakeup(state)`函数，唤醒等待队列中的第一个进程。如果这个进程的state与给定的state进行and操作之后，结果不为0,则唤醒它。

&emsp;&emsp;返回值：如果有进程被唤醒，则返回true，否则返回false。

### 1.2.3 其它API

| 函数名   | 解释           |
| ----- | ------------ |
| len() | 返回等待队列中的进程数量 |


