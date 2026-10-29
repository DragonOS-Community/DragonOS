# kthread Kernel Threads

The kernel thread module is implemented in `process/kthread.rs`, providing support for kernel threads and related functionalities. Kernel threads act as the "avatars" of the kernel, enhancing the system's parallelism and fault tolerance capabilities.

## Principles

Each kernel thread runs in kernel mode, executing its specific tasks.

The creation of a kernel thread is achieved by calling the `KernelThreadMechanism::create()` or `KernelThreadMechanism::create_and_run()` function, which sends a creation task to the `kthreadd` daemon thread. In other words, the creation of a kernel thread is ultimately handled by `kthread_daemon`.

After a kernel thread is created, it defaults to a sleeping state. To wake it up, the `ProcessManager::wakeup` function should be used.

When other kernel modules wish to stop a kernel thread, they can call the `KernelThreadMechanism::stop()` function to wait for the thread to exit, then obtain the return value and clean up the thread's PCB (Process Control Block).

Kernel threads should frequently check the result of `KernelThreadMechanism::should_stop()` to determine whether they should exit. When it is detected that the thread needs to exit, it can return a return code to terminate itself. (Note: resource cleanup is important)
