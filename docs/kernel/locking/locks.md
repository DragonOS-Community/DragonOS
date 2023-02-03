# 锁的类型及其规则

## 简介

&emsp;&emsp;DragonOS内核实现了一些锁，大致可以分为两类：

- 休眠锁
- 自旋锁

## 锁的类型

### 休眠锁

&emsp;&emsp;休眠锁只能在可抢占的上下文之中被获取。

&emsp;&emsp;在DragonOS之中，实现了以下的休眠锁：

- semaphore
- mutex_t

### 自旋锁

- spinlock_t
- {ref}`RawSpinLock <_spinlock_doc_rawspinlock>`（Rust版本的spinlock_t，但与spinlock_t不兼容）
- {ref}`SpinLock <_spinlock_doc_spinlock>` —— 在RawSpinLock的基础上，封装了一层守卫(Guard), 将锁及其要保护到的数据绑定在一个结构体内，并能在编译期避免未加锁就访问数据的问题。

&emsp;&emsp;进程在获取自旋锁后，将改变pcb中的锁变量持有计数，从而隐式地禁止了抢占。为了获得更多灵活的操作，spinlock还提供了以下的方法：

| 后缀                       | 说明                         |
| ------------------------ | -------------------------- |
| _irq()                   | 在加锁时关闭中断/在放锁时开启中断          |
| _irqsave()/_irqrestore() | 在加锁时保存中断状态，并关中断/在放锁时恢复中断状态 |

&emsp;&emsp;当您同时需要使用自旋锁以及引用计数时，一个好的方法是：使用`lockref`. 这是一种额外的加速技术，能额外提供“无锁修改引用计数”的功能。详情请见：{ref}`lockref <_lockref>`

## 详细介绍

### 自旋锁的详细介绍

&emsp;&emsp;关于自旋锁的详细介绍，请见文档：{ref}`自旋锁 <_spinlock_doc>`

### semaphore信号量

&emsp;&emsp;semaphore信号量是基于计数实现的。

&emsp;&emsp;当可用资源不足时，尝试对semaphore执行down操作的进程将会被休眠，直到资源可用。

### mutex互斥量

&emsp;&emsp;请见{ref}`Mutex文档 <_mutex_doc>`