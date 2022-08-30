# 与“等待”相关的api

&emsp;&emsp;如果几个进程需要等待某个事件发生，才能被运行，那么就需要一种“等待”的机制，以实现进程同步。

## wait_queue等待队列

&emsp;&emsp;wait_queue是一种进程同步机制，中文名为“等待队列”。它可以将当前进程挂起，并在时机成熟时，由另一个进程唤醒他们。

&emsp;&emsp;当您需要等待一个事件完成时，使用wait_queue机制能减少进程同步的开销。相比于滥用自旋锁以及信号量，或者是循环使用usleep(1000)这样的函数来完成同步，wait_queue是一个高效的解决方案。

### 简单用法

&emsp;&emsp;等待队列的使用方法主要分为以下几部分：

- 创建并初始化一个等待队列
- 使用`wait_queue_sleep_on_`系列的函数，将当前进程挂起。晚挂起的进程将排在队列的尾部。
- 通过`wait_queue_wakeup()`函数，依次唤醒在等待队列上的进程，将其加入调度队列

&emsp;&emsp;要使用wait_queue，您需要`#include<common/wait_queue.h>`，并创建一个`wait_queue_node_t`类型的变量，作为等待队列的头部。这个结构体只包含两个成员变量：

```c
typedef struct
{
    struct List wait_list;
    struct process_control_block *pcb;
} wait_queue_node_t;
```

&emsp;&emsp;对于等待队列，这里有一个好的命名方法：

```c
wait_queue_node_t wq_keyboard_interrupt_received;
```

&emsp;&emsp;这样的命名方式能增加代码的可读性，更容易让人明白这里到底在等待什么。

### 初始化等待队列

&emsp;&emsp;函数`wait_queue_init(wait_queue_node_t *wait_queue, struct process_control_block *pcb)`提供了初始化wait_queue结点的功能。

&emsp;&emsp;当您初始化队列头部时，您仅需要将wait_queue首部的结点指针传入，第二个参数请设置为NULL



### 将结点插入等待队列

&emsp;&emsp;您可以使用以下函数，将当前进程挂起，并插入到指定的等待队列。这些函数大体功能相同，只是在一些细节上有所不同。

| 函数名                                 | 解释                                                          |
| ----------------------------------- | ----------------------------------------------------------- |
| wait_queue_sleep_on()               | 将当前进程挂起，并设置挂起状态为PROC_UNINTERRUPTIBLE                        |
| wait_queue_sleep_on_unlock()        | 将当前进程挂起，并设置挂起状态为PROC_UNINTERRUPTIBLE。待当前进程被插入等待队列后，解锁给定的自旋锁 |
| wait_queue_sleep_on_interriptible() | 将当前进程挂起，并设置挂起状态为PROC_INTERRUPTIBLE                          |

### 从等待队列唤醒一个进程

&emsp;&emsp;您可以使用`void wait_queue_wakeup(wait_queue_node_t * wait_queue_head, int64_t state);`函数，从指定的等待队列中，唤醒第一个挂起时的状态与指定的`state`相同的进程。

&emsp;&emsp;当没有符合条件的进程时，将不会唤醒任何进程，仿佛无事发生。
