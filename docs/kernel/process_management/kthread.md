# kthread 内核线程

&emsp;&emsp;内核线程模块实现在`process/kthread.rs`中，提供对内核线程的及支持功能。内核线程作为内核的“分身”，能够提升系统的并行化程度以及故障容错能力。

## 原理

&emsp;&emsp;每个内核线程都运行在内核态，执行其特定的任务。

&emsp;&emsp;内核线程的创建是通过调用`KernelThreadMechanism::create()`或者`KernelThreadMechanism::create_and_run()`函数，向`kthreadd`守护线程发送创建任务来实现的。也就是说，内核线程的创建，最终是由`kthread_daemon`来完成。

&emsp;&emsp;当内核线程被创建后，默认处于睡眠状态，要使用`ProcessManager::wakeup`函数将其唤醒。

&emsp;&emsp;当内核其他模块想要停止一个内核线程的时候，可以调用`KernelThreadMechanism::stop()`函数，等待内核线程的退出，然后获得返回值并清理内核线程的pcb。

&emsp;&emsp;内核线程应当经常检查`KernelThreadMechanism::should_stop()`的结果，以确定其是否要退出。当检测到需要退出时，内核线程返回一个返回码，即可退出。（注意资源的清理）
