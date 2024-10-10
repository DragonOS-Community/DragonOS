# 完全公平调度器相关的api

&emsp;&emsp; CFS（Completely Fair Scheduler），顾名思义，完全公平调度器。CFS作为主线调度器之一，也是最典型的O(1)调度器之一

## 1. CFSQueue 介绍

&emsp;&emsp; CFSQueue是用来存放普通进程的调度队列，每个CPU维护一个CFSQueue，主要使用Vec作为主要存储结构来实现。

### 1.1 主要函数
1. enqueue(): 将pcb入队列
2. dequeue(): 将pcb从调度队列中弹出,若队列为空，则返回IDLE进程的pcb
3. sort(): 将进程按照虚拟运行时间的升序进行排列

## 2. SchedulerCFS 介绍

&emsp;&emsp; CFS调度器类，主要实现了CFS调度器类的初始化以及调度功能函数。

### 2.1 主要函数

1. sched(): 是对于Scheduler trait的sched()实现，是普通进程进行调度时的逻辑处理，该函数会返回接下来要执行的pcb，若没有符合要求的pcb，返回None
2. enqueue(): 同样是对于Scheduler trait的sched()实现，将一个pcb加入调度器的调度队列
3. update_cpu_exec_proc_jiffies(): 更新这个cpu上，这个进程的可执行时间。
4. timer_update_jiffies(): 时钟中断到来时，由sched的core模块中的函数，调用本函数，更新CFS进程的可执行时间

