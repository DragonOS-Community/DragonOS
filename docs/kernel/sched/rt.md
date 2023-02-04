# 实时进程调度器相关的api

&emsp;&emsp; RT（realtime scheduler），实时调度器。实时调度是为了完成实时处理任务而分配CPU的调度方法。

&emsp;&emsp;DragonOS的进程分为“实时进程”和“普通进程”两类；实时进程的优先级高于普通进程，如果当前的系统的执行队列中有“实时进程”，RT调度器会优先选择实时进程；如果队列中会有多个实时进程，调度器会选择优先级最高的实时进程来执行；


## 1. RTQueue 介绍

&emsp;&emsp; RTQueue是用来存放state为running的实时进程的调度队列，每个CPU维护一个RTQueue，主要使用Vec作为主要存储结构来实现。

### 1.1 主要函数
1. enqueue(): 将pcb入队列
2. dequeue(): 将pcb出队列

## 2. SchedulerRT 介绍

&emsp;&emsp; RT调度器类，主要实现了RT调度器类的初始化以及调度功能函数。

### 2.1 主要函数
1. pick_next_task_rt(): 获取当前CPU中的第一个需要执行的RT pcb
2. sched(): 是对于Scheduler trait的sched()实现，是实时进程进行调度时的逻辑处理，该函数会返回接下来要执行的pcb，若没有符合要求的pcb，返回None
3. enqueue(): 同样是对于Scheduler trait的sched()实现，将一个pcb加入调度器的调度队列

### 2.2 内核调度策略
&emsp;&emsp; 目前在DragonOS中，主要的调度策略有SCHED_NORMAL 策略 | SCHED_FIFO 策略 | SCHED_RT 策略，具体的调度策略为：
1. SCHED_NORMAL 策略：
SCHED_NORMAL 是“绝对公平调度策略”，该策略的进程使用CFS进行调度。

2. SCHED_FIFO 策略：
SCHED_FIFO是“实时进程调度策略”，这是一种先进先出的调度策略，该策略不涉及到CPU时间片机制，在没有更高优先级进程的前提下，只能等待其他进程主动释放CPU资源；
在SCHED_FIFO策略中，被调度器调度运行的进程，其运行时长不受限制，可以运行任意长的时间。

3. SCHED_RR 策略：
SCHED_RR是“实时进程调度策略”，使用的是时间片轮转机制，对应进程的time_slice会在运行时减少，进程使用完CPU时间片后，会加入该CPU的与该进程优先级相同的执行队列中。
同时，释放CPU资源，CPU的使用权会被分配给下一个执行的进程

## 3. Q&A
&emsp;&emsp; 几种常用的方法
1. 如何创建实时进程

    ```c
    struct process_control_block *pcb_name = kthread_run_rt(&fn_name, NULL, "test create rt pcb");
    ```
    其中kthread_run_rt，是创建内核实时线程的宏

2. pcb中涉及到实时进程的字段含义
    1. policy：实时进程的策略，目前有：SCHED_FIFO与SCHED_RR
    2. priority: 实时进程的优先级，范围为0-99，数字越大，表示优先级越高
    3. rt_time_slice: 实时进程的时间片，默认为100，随着CPU运行而减少，在rt_time_slice为0时，将时间片赋初值并将该进程加入执行队列。

3. 如何实时进程存储队列
    - 目前是使用Vec来保存，因为具体实现的逻辑原因，目前的入队列和出队列都是对队尾的操作，因此会有如下现象：系统中有多个优先级相同的实时进程等待运行时，会出现饥饿现象，也即上一个因为时间片耗尽的进程会在下一个执行，造成同优先级等待的进程饥饿。

4. todo
    1. 将存储实时进程的队列使用双向链表存储（或者其他办法解决上述的饥饿问题）
    2. 目前的实时调度是针对单CPU的，需要实现多CPU的实时调度