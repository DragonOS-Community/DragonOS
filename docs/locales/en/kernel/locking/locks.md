:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/locking/locks.md

- Translation time: 2025-05-19 01:41:26

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Types of Locks and Their Rules

## Introduction

&emsp;&emsp;The DragonOS kernel implements several types of locks, which can be broadly categorized into two types:

- Sleepable locks
- Spin locks

## Types of Locks

### Sleepable Locks

&emsp;&emsp;Sleepable locks can only be acquired in a context that is preemptible.

&emsp;&emsp;In DragonOS, the following sleepable locks are implemented:

- semaphore
- mutex_t

### Spin Locks

- spinlock_t
- {ref}`RawSpinLock <_spinlock_doc_rawspinlock>` (Rust version of spinlock_t, but incompatible with spinlock_t)
- {ref}`SpinLock <_spinlock_doc_spinlock>` —— Built on top of RawSpinLock, it wraps a guard, binding the lock and the data it protects into a single structure. This allows for compile-time checks to prevent accessing data without holding the lock.

&emsp;&emsp;When a process acquires a spin lock, it changes the lock count in the PCB, thereby implicitly disabling preemption. To provide more flexible operations, spinlock also provides the following methods:

| Suffix                | Description                                             |
|----------------------|--------------------------------------------------------|
| _irq()               | Disable interrupts when acquiring the lock, enable them when releasing |
| _irqsave()/_irqrestore() | Save the interrupt state when acquiring the lock, and restore it when releasing |

## Detailed Introduction

### Detailed Introduction to Spin Locks

&emsp;&emsp;For a detailed introduction to spin locks, please refer to the document: {ref}`自旋锁 <_spinlock_doc>`

### Semaphore

&emsp;&emsp;A semaphore is implemented based on a counter.

&emsp;&emsp;When the available resources are insufficient, a process attempting to perform a down operation on the semaphore will be put to sleep until the resources become available.

### Mutex

&emsp;&emsp;Please refer to {ref}`Mutex文档 <_mutex_doc>`
