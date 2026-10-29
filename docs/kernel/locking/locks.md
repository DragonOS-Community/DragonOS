# Types of Locks and Their Rules

## Introduction

The DragonOS kernel implements several types of locks, which can be broadly categorized into two types:

- Sleepable locks
- Spinlocks

## Lock Types

### Sleepable Locks

Sleepable locks can only be acquired in preemptible contexts.

In DragonOS, the following sleepable locks are implemented:

- semaphore
- rwsem
- mutex_t

### Spinlocks

- spinlock_t
- [RawSpinLock ](/kernel/locking/spinlock.html#_spinlock_doc_rawspinlock) (Rust version of spinlock_t, but incompatible with spinlock_t)
- [SpinLock ](/kernel/locking/spinlock.html#_spinlock_doc_spinlock) — Built on RawSpinLock, it encapsulates a Guard layer, binding the lock and the data it protects within a single structure, and prevents accessing data without locking at compile time.

When a process acquires a spinlock, it modifies the lock variable holding count in the PCB, thereby implicitly disabling preemption. For more flexible operations, spinlocks also provide the following methods:

| Suffix                     | Description                                                |
| ------------------------ | --------------------------------------------------- |
| _irq()                   | Disables interrupts when acquiring the lock / Enables interrupts when releasing the lock |
| _irqsave()/_irqrestore() | Saves interrupt state and disables interrupts when acquiring the lock / Restores interrupt state when releasing the lock |

## Detailed Descriptions

### Detailed Description of Spinlocks

For detailed information about spinlocks, please refer to the document: [Spinlock](/kernel/locking/spinlock.html#_spinlock_doc)

### Semaphore

The semaphore is implemented based on counting.

When available resources are insufficient, a process attempting to perform a down operation on the semaphore will be put to sleep until resources become available.

### Mutex

Please refer to [Mutex documentation](/kernel/locking/mutex.html#_mutex_doc)
