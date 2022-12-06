# 与“等待”相关的api

&emsp;&emsp;如果几个进程需要等待某个事件发生，才能被运行，那么就需要一种“等待”的机制，以实现进程同步。

## 一. wait_queue等待队列

&emsp;&emsp;wait_queue是一种进程同步机制，中文名为“等待队列”。它可以将当前进程挂起，并在时机成熟时，由另一个进程唤醒他们。

&emsp;&emsp;当您需要等待一个事件完成时，使用wait_queue机制能减少进程同步的开销。相比于滥用自旋锁以及信号量，或者是循环使用usleep(1000)这样的函数来完成同步，wait_queue是一个高效的解决方案。

:::{warning}
`wait_queue.h`中的等待队列的实现并没有把队列头独立出来，同时没有考虑为等待队列加锁。所以在后来的开发中加入了`wait_queue_head_t`的队列头实现，实质上就是链表+自旋锁。它与`wait_queue.h`中的队列`wait_queue_node_t`是兼容的，当你使用`struct wait_queue_head`作为队列头时，你同样可以使用等待队列添加节点的函数。
:::

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


------------------------------------------------------------
&emsp;&emsp; 
&emsp;&emsp; 
&emsp;&emsp; 


## 二. wait_queue_head等待队列头

&emsp;&emsp; 数据结构定义如下：

```c
typedef struct
{
    struct List wait_list;
    spinlock_t lock;  // 队列需要有一个自旋锁,虽然目前内部并没有使用,但是以后可能会用.
} wait_queue_head_t;
```

&emsp;&emsp; 等待队列头的使用逻辑与等待队列实际是一样的，因为他同样也是等待队列的节点(仅仅多了一把锁)。且wait_queue_head的函数基本上与wait_queue一致，只不过多了\*\*\*\_with\_node\_\*\*\*的字符串。

&emsp;&emsp; 同时，wait_queue.h文件中提供了很多的宏，可以方便您的工作。

### 提供的宏 
| 宏                               | 解释                                                          |
| ----------------------------------- | ----------------------------------------------------------- |
| DECLARE_WAIT_ON_STACK(name, pcb)             | 在栈上声明一个wait_queue节点，同时把pcb所代表的进程与该节点绑定 |
| DECLARE_WAIT_ON_STACK_SELF(name)     | 传在栈上声明一个wait_queue节点，同时当前进程(即自身进程)与该节点绑定 |
| DECLARE_WAIT_ALLOC(name, pcb)  | 使用`kzalloc`声明一个wait_queue节点，同时把pcb所代表的进程与该节点绑定，请记得使用kfree释放空间 |
| DECLARE_WAIT_ALLOC_SELF(name)      | 使用`kzalloc`声明一个wait_queue节点，同时当前进程(即自身进程)与该节点绑定，请记得使用kfree释放空间 |



### 创建等待队列头
&emsp;&emsp; 您可以直接调用宏
```c
DECLARE_WAIT_QUEUE_HEAD(m_wait_queue_head);  // 在栈上声明一个队列头变量
```
&emsp;&emsp; 也可以手动声明
```c
struct wait_queue_head_t m_wait_queue_head = {0}; 
wait_queue_head_init(&m_wait_queue_head);
```


### 将结点插入等待队列

| 函数名                                 | 解释                                                          |
| ----------------------------------- | ----------------------------------------------------------- |
| wait_queue_sleep_with_node(wait_queue_head_t *head, wait_queue_node_t *wait_node)              | 传入一个等待队列节点，并设置该节点的挂起状态为PROC_UNINTERRUPTIBLE                        |
| wait_queue_sleep_with_node_unlock(wait_queue_head_t *q, wait_queue_node_t *wait, void *lock)      | 传入一个等待队列节点，将该节点的pcb指向的进程挂起，并设置挂起状态为PROC_UNINTERRUPTIBLE。待当前进程被插入等待队列后，解锁给定的自旋锁 |
| wait_queue_sleep_with_node_interriptible(wait_queue_head_t *q, wait_queue_node_t *wait) | 传入一个等待队列节点，将该节点的pcb指向的进程挂起，并设置挂起状态为PROC_INTERRUPTIBLE                          |



### 从等待队列唤醒一个进程
&emsp;&emsp; 在`wait_queue.h`中的`wait_queue_wakeup`函数直接kfree掉了wait_node节点。对于在栈上的wait_node,您可以选择`wait_queue_wakeup_on_stack(wait_queue_head_t *q, int64_t state)`来唤醒队列里面的队列头节点。

------------------------------------------------------------
&emsp;&emsp; 
&emsp;&emsp; 
&emsp;&emsp; 

## 三. completion完成量


### 简单用法
&emsp;&emsp;完成量的使用方法主要分为以下几部分：

- 声明一个完成量(可以在栈中/使用kmalloc/使用数组)
- 使用wait_for_completion等待事件完成
- 使用complete唤醒等待的进程

&emsp;&emsp; 等待操作
```c
void wait_fun() {
    DECLARE_COMPLETION_ON_STACK(comp);  // 声明一个completion 

    // .... do somethind here 
    // 大部分情况是你使用kthread_run()创建了另一个线程
    // 你需要把comp变量传给这个线程, 然后当前线程就会等待他的完成

    if (!try_wait_for_completion(&comp))  // 进入等待
        wait_for_completion(&comp);
}
```

&emsp;&emsp; 完成操作
```c
void kthread_fun(struct completion *comp) {
    // ...... 做一些事  .......
    // 这里你确定你完成了目标事件

    complete(&comp);
    // 或者你使用complete_all
    complete_all(&comp);
}
```

### 更多用法
&emsp;&emsp; kernel/sched/completion.c文件夹中,你可以看到 __test 开头的几个函数,他们是completion模块的测试代码,基本覆盖了completion的大部分函数.你可以在这里查询函数使用方法.

### 初始化完成量
&emsp;&emsp; 函数`completion_init(struct completion *x)`提供了初始化completion的功能。当你使用`DECLARE_COMPLETION_ON_STACK`来创建(在栈上创建)的时候,会自动初始化.

### 关于完成量的wait系列函数

| 函数名                                 | 解释                                                          |
| ----------------------------------- | ----------------------------------------------------------- |
| wait_for_completion(struct completion *x)       | 将当前进程挂起，并设置挂起状态为PROC_UNINTERRUPTIBLE。                     |
| wait_for_completion_timeout(struct completion *x, long timeout)  | 将当前进程挂起，并设置挂起状态为PROC_UNINTERRUPTIBLE。当等待timeout时间(jiffies时间片)之后,自动唤醒进程。 |
| wait_for_completion_interruptible(struct completion *x) | 将当前进程挂起，并设置挂起状态为PROC_INTERRUPTIBLE。                          |
| wait_for_completion_interruptible_timeout(struct completion *x, long timeout) | 将当前进程挂起，并设置挂起状态为PROC_INTERRUPTIBLE。当等待timeout时间(jiffies时间片)之后,自动唤醒进程。                         |
| wait_for_multicompletion(struct completion x[], int n)| 将当前进程挂起，并设置挂起状态为PROC_UNINTERRUPTIBLE。(等待数组里面的completion的完成)                     |



### 关于完成量的complete系列函数

| 函数名                                 | 解释                                                          |
| ----------------------------------- | ----------------------------------------------------------- |
| complete(struct completion *x)             | 表明一个事件被完成,从等待队列中唤醒一个进程                     |
| complete_all(struct completion *x)      | 表明与该completion有关的事件被标记为永久完成,并唤醒等待队列中的所有进程 |


### 其他用于查询信息的函数
| 函数名                                 | 解释                                                          |
| ----------------------------------- | ----------------------------------------------------------- |
| completion_done(struct completion *x)            | 查询completion的done变量是不是大于0，如果大于0，返回true；否则返回false。在等待前加上这个函数有可能加速？(暂未经过实验测试，有待证明)                     |
| try_wait_for_completion(struct completion *x)   | 查询completion的done变量是不是大于0，如果大于0，返回true(同时令done-=1)；否则返回false。在等待前加上这个函数有可能加速？（该函数和`completion_done`代码逻辑基本一致，但是会主动令completion的done变量减1）   |


