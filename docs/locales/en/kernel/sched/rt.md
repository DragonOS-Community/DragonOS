:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/sched/rt.md

- Translation time: 2025-05-19 01:41:17

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# APIs Related to Real-Time Process Scheduler

&emsp;&emsp; RT (Real-Time Scheduler), real-time scheduler. Real-time scheduling is a method of allocating CPU resources to complete real-time processing tasks.

&emsp;&emsp; In DragonOS, processes are divided into two categories: "real-time processes" and "normal processes". Real-time processes have higher priority than normal processes. If there are real-time processes in the current system's execution queue, the RT scheduler will prioritize selecting a real-time process. If there are multiple real-time processes in the queue, the scheduler will select the one with the highest priority to execute.

## 1. Introduction to RTQueue

&emsp;&emsp; RTQueue is a scheduling queue used to store real-time processes with a state of running. Each CPU maintains its own RTQueue, and it primarily uses Vec as the main storage structure to implement this.

### 1.1 Main Functions
1. enqueue(): Add the PCB to the queue
2. dequeue(): Remove the PCB from the queue

## 2. Introduction to SchedulerRT

&emsp;&emsp; RT Scheduler class, which mainly implements the initialization and scheduling function of the RT scheduler.

### 2.1 Main Functions
1. pick_next_task_rt(): Get the first PCB that needs to be executed on the current CPU
2. sched(): Implementation of the sched() function for the Scheduler trait. This function handles the logic for scheduling real-time processes and returns the PCB to be executed next. If no suitable PCB is found, it returns None.
3. enqueue(): Also an implementation of the sched() function for the Scheduler trait, which adds a PCB to the scheduler's scheduling queue

### 2.2 Kernel Scheduling Policies
&emsp;&emsp; Currently, the main scheduling policies in DragonOS are SCHED_NORMAL policy | SCHED_FIFO policy | SCHED_RT policy. The specific scheduling policies are as follows:
1. SCHED_NORMAL policy:
SCHED_NORMAL is an "absolute fair scheduling policy", and processes using this policy are scheduled using CFS.

2. SCHED_FIFO policy:
SCHED_FIFO is a "real-time process scheduling policy", which is a first-in-first-out scheduling strategy. This policy does not involve the CPU time slice mechanism. Without a higher-priority process, the process can only wait for other processes to release CPU resources. In the SCHED_FIFO policy, the running time of the process scheduled by the scheduler is not limited and can run for an arbitrary length of time.

3. SCHED_RR policy:
SCHED_RR is a "real-time process scheduling policy", which uses a time-slice rotation mechanism. The time_slice of the corresponding process will decrease during execution. Once the process uses up its CPU time slice, it will be added to the execution queue of the same priority on the same CPU. At the same time, the CPU resource is released, and the CPU usage will be allocated to the next process to execute.

## 3. Q&A
&emsp;&emsp; Several commonly used methods
1. How to create a real-time process

    ```c
    struct process_control_block *pcb_name = kthread_run_rt(&fn_name, NULL, "test create rt pcb");
    ```
    Where kthread_run_rt is a macro for creating a kernel real-time thread

2. Meaning of fields related to real-time processes in the PCB
    1. policy: The scheduling policy of the real-time process, currently SCHED_FIFO and SCHED_RR
    2. priority: The priority of the real-time process, ranging from 0 to 99. The larger the number, the higher the priority
    3. rt_time_slice: The time slice of the real-time process. The default is 100, which decreases as the CPU runs. When rt_time_slice reaches 0, the time slice is reset to its initial value, and the process is added to the execution queue

3. How real-time processes are stored in the queue
    - Currently, Vec is used to store the processes. Due to the specific implementation logic, the enqueue and dequeue operations are performed at the end of the queue. Therefore, the following phenomenon may occur: when there are multiple real-time processes with the same priority waiting to run, a starvation phenomenon may occur. That is, the process that ran out of its time slice will be executed next, causing starvation among the processes with the same priority waiting.

4. Todo
    1. Use a doubly linked list (or other methods) to store the queue of real-time processes to solve the above starvation issue
    2. Currently, the real-time scheduling is for a single CPU. It needs to be implemented for multiple CPUs
    3. Implement the bandwidth allocation ratio between real-time processes and normal processes
    4. Achieve load balancing between multiple CPUs
