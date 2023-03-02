# kthread 内核线程

&emsp;&emsp;内核线程模块定义在`common/kthread.h`中，提供对内核线程的及支持功能。内核线程作为内核的“分身”，能够提升系统的并行化程度以及故障容错能力。

## 原理

&emsp;&emsp;每个内核线程都运行在内核态，执行其特定的任务。

&emsp;&emsp;内核线程的创建是通过调用`kthread_create()`或者`kthread_run()`宏，向`kthreadd`守护线程发送创建任务来实现的。也就是说，内核线程的创建，最终是由`kthreadd`来完成。

&emsp;&emsp;当内核线程被创建后，虽然会加入调度队列，但是当其被第一次调度，执行引导程序`kthread()`后，将进入休眠状态。直到其他模块使用`process_wakeup()`，它才会真正开始运行。

&emsp;&emsp;当内核其他模块想要停止一个内核线程的时候，可以调用`kthread_stop()`函数。该函数将会置位内核线程的`worker_private`中的`KTHREAD_SHOULD_STOP`标志位，并等待内核线程的退出，然后获得返回值并清理内核线程的pcb。

&emsp;&emsp;内核线程应当经常检查`KTHREAD_SHOULD_STOP`标志位，以确定其是否要退出。当检测到该标志位被置位时，内核线程应当完成数据清理工作，并调用`kthread_exit()`或直接返回一个返回码，以退出内核线程。

## 创建内核线程

### kthread_create()

#### 原型

&emsp;&emsp;`kthread_create(thread_fn, data, name_fmt, arg...)`

#### 简介

&emsp;&emsp;在当前NUMA结点上创建一个内核线程（DragonOS目前暂不支持NUMA，因此node可忽略。）

&emsp;&emsp;请注意，该宏会创建一个内核线程，并将其设置为停止状态.

#### 参数

**thread_fn**

&emsp;&emsp;该内核线程要执行的函数

**data**

&emsp;&emsp;传递给 *thread_fn* 的参数数据

**name_fmt**

&emsp;&emsp;printf-style format string for the thread name

**arg**

&emsp;&emsp;name_fmt的参数

#### 返回值

&emsp;&emsp;创建好的内核线程的pcb

### kthread_run()

#### 原型

&emsp;&emsp;`kthread_run(thread_fn, data, name_fmt, ...)`

#### 简介

&emsp;&emsp;创建内核线程并加入调度队列。

&emsp;&emsp;该宏定义是`kthread_create()`的简单封装，提供创建了内核线程后，立即运行的功能。

### kthread_run_rt()

#### 原型

&emsp;&emsp;`kthread_run_rt(thread_fn, data, name_fmt, ...)`

#### 简介

&emsp;&emsp;创建内核实时线程并加入调度队列。

&emsp;&emsp;类似`kthread_run()`，该宏定义也是`kthread_create()`的简单封装，提供创建了内核实时线程后，在设置实时进程的参数后，立即运行的功能。

## 停止内核线程

### kthread_stop()

#### 原型

&emsp;&emsp;`int kthread_stop(struct process_control_block * pcb)`

#### 简介

&emsp;&emsp;当外部模块希望停止一个内核线程时，调用该函数，向kthread发送停止消息，请求其结束。并等待其退出，返回内核线程的退出返回值。

#### 参数

**pcb**

&emsp;&emsp;内核线程的pcb

#### 返回值

&emsp;&emsp;内核线程的退出返回码。

### kthread_should_stop()


#### 原型

&emsp;&emsp;`bool kthread_should_stop(void)`

#### 简介

&emsp;&emsp;内核线程可以调用该函数得知是否有其他进程请求结束当前内核线程。

#### 返回值

&emsp;&emsp;一个bool变量


| 值         | 解释                      |
| ---------- | ----------------------- |
| true       | 有其他进程请求结束该内核线程   |
| false       | 该内核线程没有收到停止消息  |

### kthread_exit()

#### 原型

&emsp;&emsp;`void kthread_exit(long result)`

#### 简介

&emsp;&emsp;让当前内核线程退出，并返回result参数给kthread_stop()函数。

#### 参数

**result**

&emsp;&emsp;内核线程的退出返回码
