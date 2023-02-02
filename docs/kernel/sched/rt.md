# 实时进程调度器相关的api

&emsp;&emsp; RT（realtime scheduler），实时调度器。实时调度是为了完成实时处理任务而分配CPU的调度方法

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

