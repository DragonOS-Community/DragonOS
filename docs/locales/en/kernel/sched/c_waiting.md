:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/sched/c_waiting.md

- Translation time: 2025-05-19 01:43:09

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# APIs Related to "Waiting" (C Language)

:::{warning}

As the kernel evolves, we will gradually replace the C language waiting mechanism with the Rust language waiting mechanism. During this process, we will retain both the C and Rust waiting mechanisms to allow for comparison during development.
Once the timing is ripe, we will gradually remove the C language waiting mechanism.
:::

&emsp;&emsp; If several processes need to wait for an event to occur before they can be executed, a "waiting" mechanism is required to achieve process synchronization.

## I. wait_queue Waiting Queue

&emsp;&emsp; wait_queue is a process synchronization mechanism, known as "waiting queue" in Chinese. It can suspend the current process and wake them up when the time is ripe, by another process.

&emsp;&emsp; When you need to wait for an event to complete, using the wait_queue mechanism can reduce the overhead of process synchronization. Compared to overusing spinlocks and semaphores, or repeatedly calling usleep(1000) functions for synchronization, wait_queue is an efficient solution.

:::{warning}
The implementation of the wait_queue in `wait_queue.h` does not have an independent queue head and does not consider locking the wait_queue. Therefore, in later development, the queue head implementation in `wait_queue_head_t` was added, which is essentially a linked list plus a spinlock. It is compatible with the queue `wait_queue_node_t` in `wait_queue.h`. When you use `struct wait_queue_head` as the queue head, you can still use the functions to add nodes to the waiting queue.
:::

### Simple Usage

&emsp;&emsp; The usage of a waiting queue mainly includes the following parts:

- Creating and initializing a waiting queue
- Using the `wait_queue_sleep_on_` series of functions to suspend the current process. Processes that are suspended later will be placed at the end of the queue.
- Using the `wait_queue_wakeup()` function to wake up processes waiting in the waiting queue and add them to the scheduling queue

&emsp;&emsp; To use wait_queue, you need to `#include<common/wait_queue.h>`, and create a `wait_queue_node_t` type variable as the head of the waiting queue. This structure contains only two member variables:

```c
typedef struct
{
    struct List wait_list;
    struct process_control_block *pcb;
} wait_queue_node_t;
```

&emsp;&emsp; For the waiting queue, there is a good naming method:

```c
wait_queue_node_t wq_keyboard_interrupt_received;
```

&emsp;&emsp; This naming convention increases code readability and makes it easier to understand what the code is waiting for.

### Initializing the Waiting Queue

&emsp;&emsp; The function `wait_queue_init(wait_queue_node_t *wait_queue, struct process_control_block *pcb)` provides the functionality to initialize a wait_queue node.

&emsp;&emsp; When you initialize the queue head, you only need to pass the pointer to the wait_queue head node, and set the second parameter to NULL.

### Inserting a Node into the Waiting Queue

&emsp;&emsp; You can use the following functions to suspend the current process and insert it into the specified waiting queue. These functions have similar overall functions, but differ in some details.

| Function Name                         | Explanation                                                       |
| ----------------------------------- | ---------------------------------------------------------------- |
| wait_queue_sleep_on()               | Suspends the current process and sets the suspension state to PROC_UNINTERRUPTIBLE |
| wait_queue_sleep_on_unlock()        | Suspends the current process and sets the suspension state to PROC_UNINTERRUPTIBLE. After the current process is inserted into the waiting queue, it unlocks the given spinlock |
| wait_queue_sleep_on_interriptible() | Suspends the current process and sets the suspension state to PROC_INTERRUPTIBLE |

### Waking Up a Process from the Waiting Queue

&emsp;&emsp; You can use the `void wait_queue_wakeup(wait_queue_node_t * wait_queue_head, int64_t state);` function to wake up the first process in the specified waiting queue that has a suspension state matching the specified `state`.

&emsp;&emsp; If there are no matching processes, no process will be woken up, as if nothing happened.

------------------------------------------------------------
&emsp;&emsp; 
&emsp;&emsp; 
&emsp;&emsp; 

## II. wait_queue_head Waiting Queue Head

&emsp;&emsp; The data structure is defined as follows:

```c
typedef struct
{
    struct List wait_list;
    spinlock_t lock;  // 队列需要有一个自旋锁,虽然目前内部并没有使用,但是以后可能会用.
} wait_queue_head_t;
```

&emsp;&emsp; The usage logic of the waiting queue head is the same as the waiting queue itself, because it is also a node of the waiting queue (just with an additional lock). The functions of wait_queue_head are basically the same as those of wait_queue, except that they include the string \*\*\*\_with\_node\_\*\*\*.

&emsp;&emsp; Meanwhile, the wait_queue.h file provides many macros that can make your work easier.

### Provided Macros
| Macro                               | Explanation                                                       |
| ----------------------------------- | ---------------------------------------------------------------- |
| DECLARE_WAIT_ON_STACK(name, pcb)             | Declare a wait_queue node on the stack, and bind the pcb-represented process to this node |
| DECLARE_WAIT_ON_STACK_SELF(name)     | Declare a wait_queue node on the stack, and bind the current process (i.e., the process itself) to this node |
| DECLARE_WAIT_ALLOC(name, pcb)  | Use `kzalloc` to declare a wait_queue node, and bind the pcb-represented process to this node. Remember to use kfree to release the space |
| DECLARE_WAIT_ALLOC_SELF(name)      | Use `kzalloc` to declare a wait_queue node, and bind the current process (i.e., the process itself) to this node. Remember to use kfree to release the space |

### Creating a Waiting Queue Head
&emsp;&emsp; You can directly call the macro
```c
DECLARE_WAIT_QUEUE_HEAD(m_wait_queue_head);  // 在栈上声明一个队列头变量
```
&emsp;&emsp; Or manually declare
```c
struct wait_queue_head_t m_wait_queue_head = {0}; 
wait_queue_head_init(&m_wait_queue_head);
```

### Inserting a Node into the Waiting Queue

| Function Name                         | Explanation                                                       |
| ----------------------------------- | ---------------------------------------------------------------- |
| wait_queue_sleep_with_node(wait_queue_head_t *head, wait_queue_node_t *wait_node)              | Pass in a waiting queue node, and set the suspension state of the node to PROC_UNINTERRUPTIBLE |
| wait_queue_sleep_with_node_unlock(wait_queue_head_t *q, wait_queue_node_t *wait, void *lock)      | Pass in a waiting queue node, suspend the process pointed to by the node's pcb, and set the suspension state to PROC_UNINTERRUPTIBLE. After the current process is inserted into the waiting queue, unlock the given spinlock |
| wait_queue_sleep_with_node_interriptible(wait_queue_head_t *q, wait_queue_node_t *wait) | Pass in a waiting queue node, suspend the process pointed to by the node's pcb, and set the suspension state to PROC_INTERRUPTIBLE |

### Waking Up a Process from the Waiting Queue
&emsp;&emsp; The `wait_queue_wakeup` function in `wait_queue.h` directly kfree's the wait_node node. For stack-based wait_node, you can choose `wait_queue_wakeup_on_stack(wait_queue_head_t *q, int64_t state)` to wake up the queue head node in the queue.

------------------------------------------------------------
&emsp;&emsp; 
&emsp;&emsp; 
&emsp;&emsp; 

## III. completion Completion Count

### Simple Usage
&emsp;&emsp; The usage of completion mainly includes the following parts:

- Declare a completion (can be on the stack, using kmalloc, or using an array)
- Use wait_for_completion to wait for the event to complete
- Use complete to wake up the waiting processes

&emsp;&emsp; Waiting operation
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

&emsp;&emsp; Completion operation
```c
void kthread_fun(struct completion *comp) {
    // ...... 做一些事  .......
    // 这里你确定你完成了目标事件

    complete(&comp);
    // 或者你使用complete_all
    complete_all(&comp);
}
```

### More Usage
&emsp;&emsp; In the kernel/sched/completion.c folder, you can see several functions starting with __test__, which are test code for the completion module and cover most of the completion functions. You can refer to these functions to learn how to use them.

### Initializing Completion
&emsp;&emsp; The function `completion_init(struct completion *x)` provides the functionality to initialize a completion. When you use `DECLARE_COMPLETION_ON_STACK` to create (on the stack), it will be automatically initialized.

### Completion-related wait series functions

| Function Name                         | Explanation                                                       |
| ----------------------------------- | ---------------------------------------------------------------- |
| wait_for_completion(struct completion *x)       | Suspends the current process and sets the suspension state to PROC_UNINTERRUPTIBLE.                     |
| wait_for_completion_timeout(struct completion *x, long timeout)  | Suspends the current process and sets the suspension state to PROC_UNINTERRUPTIBLE. After waiting for timeout time (jiffies time slice), the process is automatically awakened. |
| wait_for_completion_interruptible(struct completion *x) | Suspends the current process and sets the suspension state to PROC_INTERRUPTIBLE.                          |
| wait_for_completion_interruptible_timeout(struct completion *x, long timeout) | Suspends the current process and sets the suspension state to PROC_INTERRUPTIBLE. After waiting for timeout time (jiffies time slice), the process is automatically awakened.                         |
| wait_for_multicompletion(struct completion x[], int n)| Suspends the current process and sets the suspension state to PROC_UNINTERRUPTIBLE. (Waiting for the completion of the array's completions)                     |

### Completion-related complete series functions

| Function Name                         | Explanation                                                       |
| ----------------------------------- | ---------------------------------------------------------------- |
| complete(struct completion *x)             | Indicates that an event has been completed, and wakes up one process from the waiting queue                     |
| complete_all(struct completion *x)      | Indicates that the events related to this completion are marked as permanently completed, and wakes up all processes in the waiting queue |

### Other Functions for Querying Information
| Function Name                         | Explanation                                                       |
| ----------------------------------- | ---------------------------------------------------------------- |
| completion_done(struct completion *x)            | Checks if the completion's done variable is greater than 0. If it is, returns true; otherwise, returns false. Adding this function before waiting may accelerate the process? (Not tested experimentally yet, needs further verification)                     |
| try_wait_for_completion(struct completion *x)   | Checks if the completion's done variable is greater than 0. If it is, returns true (and decrements done by 1); otherwise, returns false. Adding this function before waiting may accelerate the process? (This function has the same logic as `completion_done`, but actively decrements the completion's done variable by 1)   |
