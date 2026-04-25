:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/locking/locks.md

- Translation time: 2026-01-05 12:01:11

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Types of Locks and Their Rules

## Introduction

&emsp;&emsp;The DragonOS kernel implements several types of locks, which can be broadly categorized into two types:

- Sleepable locks
- Spinlocks

## Lock Types

### Sleepable Locks

&emsp;&emsp;Sleepable locks can only be acquired in preemptible contexts.

&emsp;&emsp;In DragonOS, the following sleepable locks are implemented:

- semaphore
- rwsem
- mutex_t

### Spinlocks

- spinlock_t
- {ref}`RawSpinLock <_spinlock_doc_rawspinlock>` (Rust version of spinlock_t, but incompatible with spinlock_t)
- {ref}`SpinLock <_spinlock_doc_spinlock>` — Built on RawSpinLock, it encapsulates a Guard layer, binding the lock and the data it protects within a single structure, and prevents accessing data without locking at compile time.

&emsp;&emsp;When a process acquires a spinlock, it modifies the lock variable holding count in the PCB, thereby implicitly disabling preemption. For more flexible operations, spinlocks also provide the following methods:

| Suffix                     | Description                                                |
| ------------------------ | --------------------------------------------------- |
| _irq()                   | Disables interrupts when acquiring the lock / Enables interrupts when releasing the lock |
| _irqsave()/_irqrestore() | Saves interrupt state and disables interrupts when acquiring the lock / Restores interrupt state when releasing the lock |

## Detailed Descriptions

### Detailed Description of Spinlocks

&emsp;&emsp;For detailed information about spinlocks, please refer to the document: {ref}`自旋锁 <_spinlock_doc>`

### Semaphore

&emsp;&emsp;The semaphore is implemented based on counting.

&emsp;&emsp;When available resources are insufficient, a process attempting to perform a down operation on the semaphore will be put to sleep until resources become available.

### Mutex

&emsp;&emsp;Please refer to {ref}`Mutex文档 <_mutex_doc>`
