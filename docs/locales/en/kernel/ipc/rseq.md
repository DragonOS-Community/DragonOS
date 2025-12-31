:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/ipc/rseq.md

- Translation time: 2025-12-27 12:36:02

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Restartable Sequences (rseq) Mechanism

## 1. Overview

Restartable Sequences (rseq, restartable sequences) is a user-space and kernel collaborative mechanism designed for efficient per-CPU data access. It allows user-space programs to safely access and modify per-CPU data structures without using traditional synchronization primitives such as locks or atomic operations.

### 1.1 Design Goals

The core objective of rseq is to provide an **optimistic concurrency** mechanism:

- User-space code can assume it won't be interrupted and directly operate on per-CPU data
- If interruption does occur (preemption, signals, etc.), the kernel redirects execution to a recovery path
- This "either complete execution or start over" semantics avoids the overhead of traditional locks

### 1.2 Typical Use Cases

- **Memory allocators**: tcmalloc, jemalloc, etc., use per-CPU caches to accelerate allocation
- **Reference counting**: per-CPU reference counts can avoid cache line contention
- **Statistical counters**: lock-free updates of per-CPU counters
- **RCU read-side critical sections**: quickly obtaining current CPU information

## 2. Core Concepts

### 2.1 Critical Section

An rseq critical section is a segment of user-space code with the following characteristics:

```
┌─────────────────────────────────────────────────────────────┐
│                     rseq 临界区                              │
│                                                             │
│  start_ip ──► ┌─────────────────────────────────┐           │
│               │  1. 读取 cpu_id                  │           │
│               │  2. 使用 cpu_id 索引 per-CPU 数据 │           │
│               │  3. 执行操作（读/改/写）          │           │
│               │  4. 提交点（commit point）        │           │
│  end_ip ────► └─────────────────────────────────┘           │
│                         │                                   │
│                         │ 被打断时跳转                       │
│                         ▼                                   │
│  abort_ip ──► ┌─────────────────────────────────┐           │
│               │  恢复/重试逻辑                    │           │
│               └─────────────────────────────────┘           │
└─────────────────────────────────────────────────────────────┘
```

- **start_ip**: Starting address of the critical section
- **post_commit_offset**: Offset from start_ip to the commit point
- **abort_ip**: Interruption recovery address, must be located outside the critical section

### 2.2 User-space Data Structure

User-space needs to maintain a `struct rseq` structure in TLS (Thread Local Storage):

| Field | Size | Description |
|-------|------|-------------|
| cpu_id_start | u32 | CPU ID when entering the critical section |
| cpu_id | u32 | Current CPU ID (updated by kernel) |
| rseq_cs | u64 | Pointer to the current critical section descriptor |
| flags | u32 | Flag bits (reserved) |
| node_id | u32 | NUMA node ID |
| mm_cid | u32 | Memory management context ID |

### 2.3 Critical Section Descriptor

`struct rseq_cs` describes a specific critical section:

| Field | Size | Description |
|-------|------|-------------|
| version | u32 | Version number, must be 0 |
| flags | u32 | Flag bits |
| start_ip | u64 | Starting address of the critical section |
| post_commit_offset | u64 | Length of the critical section |
| abort_ip | u64 | Interruption recovery address |

## 3. Working Principle

### 3.1 Registration Process

```
用户态                                    内核态
  │                                         │
  │  sys_rseq(rseq_ptr, len, 0, sig)       │
  │ ──────────────────────────────────────► │
  │                                         │ 1. 验证参数
  │                                         │ 2. 记录注册信息
  │                                         │ 3. 设置 NEED_RSEQ 标志
  │                                         │
  │  返回 0（成功）                          │
  │ ◄────────────────────────────────────── │
  │                                         │
```

### 3.2 Critical Section Execution

During normal execution, user-space code:

1. Writes the address of the critical section descriptor to `rseq->rseq_cs`
2. Reads `rseq->cpu_id` to obtain the current CPU
3. Uses this CPU ID to access per-CPU data
4. After completing operations, clears `rseq->rseq_cs`

### 3.3 Kernel Intervention Timing

The kernel checks and corrects before returning to user-space after the following events:

```
┌──────────────────────────────────────────────────────────────┐
│                    触发 rseq 处理的事件                        │
├──────────────────────────────────────────────────────────────┤
│  抢占（Preemption）                                           │
│    └─► 调度器切换进程时设置 PREEMPT 事件                        │
│                                                              │
│  信号递送（Signal Delivery）                                   │
│    └─► 设置信号帧前设置 SIGNAL 事件                            │
│                                                              │
│  CPU 迁移（Migration）                                        │
│    └─► 进程被迁移到其他 CPU 时设置 MIGRATE 事件                 │
└──────────────────────────────────────────────────────────────┘
```

### 3.4 Pre-return-to-user-space Processing

When the process is about to return to user-space, the kernel performs the following steps:

```
                    返回用户态前处理流程
                           │
                           ▼
                  ┌─────────────────┐
                  │ 检查 NEED_RSEQ  │
                  │    标志位       │
                  └────────┬────────┘
                           │ 已设置
                           ▼
                  ┌─────────────────┐
                  │  读取 rseq_cs   │
                  │  指针           │
                  └────────┬────────┘
                           │
              ┌────────────┴────────────┐
              │                         │
              ▼                         ▼
        rseq_cs == 0              rseq_cs != 0
        (不在临界区)               (在临界区)
              │                         │
              │                         ▼
              │                 ┌───────────────┐
              │                 │ 当前 IP 在    │
              │                 │ 临界区内？    │
              │                 └───────┬───────┘
              │                    是   │   否
              │              ┌──────────┴──────────┐
              │              ▼                     ▼
              │      ┌───────────────┐     ┌───────────────┐
              │      │ 修改返回地址   │     │ 清除 rseq_cs  │
              │      │ 为 abort_ip   │     │ (lazy clear)  │
              │      └───────────────┘     └───────────────┘
              │              │                     │
              └──────────────┴─────────────────────┘
                             │
                             ▼
                    ┌─────────────────┐
                    │ 更新 cpu_id 等  │
                    │ TLS 字段        │
                    └─────────────────┘
                             │
                             ▼
                      返回用户态
```

## 4. Safety Mechanisms

### 4.1 Signature Verification

During registration, the user provides a 32-bit signature value (sig), which the kernel verifies when processing the critical section:

- Reads 4 bytes at `abort_ip - 4`
- Must match the registered signature
- Prevents maliciously constructed critical section descriptors

### 4.2 Address Verification

The kernel strictly verifies all user-space addresses:

- `start_ip`, `abort_ip` must be within the user address space
- `start_ip + post_commit_offset` cannot overflow
- `abort_ip` must be outside the critical section

### 4.3 Error Handling

When the following errors are detected, the kernel sends SIGSEGV to the process:

- User memory access failure
- Signature mismatch
- Address verification failure
- Version number not equal to 0

## 5. Integration with Process Lifecycle

### 5.1 fork

- **CLONE_VM (threads)**: Child threads need to re-register rseq
- **fork (processes)**: Child processes inherit the parent's rseq registration state

### 5.2 execve

When executing a new program, the rseq registration state is cleared, and the new program needs to re-register.

### 5.3 exit

When a process exits, the rseq state is released along with the PCB, requiring no special handling.

## 6. System Call Interface

### sys_rseq

```c
long sys_rseq(struct rseq *rseq, u32 rseq_len, int flags, u32 sig);
```

**Parameters:**
- `rseq`: Address of the user-space rseq structure
- `rseq_len`: Structure length (at least 32 bytes)
- `flags`: 0 for registration, RSEQ_FLAG_UNREGISTER (1) for deregistration
- `sig`: Signature value

**Return Value:**
- Success: 0
- Failure: Negative error code

**Error Codes:**
| Error Code | Description |
|------------|-------------|
| EINVAL | Invalid parameters (length, alignment, flags, etc.) |
| EPERM | Signature mismatch |
| EBUSY | Already registered (duplicate registration with same parameters) |
| EFAULT | Invalid address |

## 7. Auxiliary Vector (auxv)

The kernel passes rseq support information to user-space through the ELF auxiliary vector:

| Type | Value | Description |
|------|-------|-------------|
| AT_RSEQ_FEATURE_SIZE | 27 | rseq structure size (32) |
| AT_RSEQ_ALIGN | 28 | rseq alignment requirement (32) |

User-space libraries (such as glibc) use this information to:
- Determine whether the kernel supports rseq
- Correctly allocate and align the rseq structure in TLS

## 8. Usage Example

The following pseudocode demonstrates a typical usage pattern of rseq:

```c
// 1. 注册 rseq
struct rseq *rseq_ptr = &__rseq_abi;  // TLS 中的 rseq 结构
syscall(SYS_rseq, rseq_ptr, sizeof(*rseq_ptr), 0, RSEQ_SIG);

// 2. 定义临界区描述符
struct rseq_cs cs = {
    .version = 0,
    .flags = 0,
    .start_ip = (uintptr_t)&&start,
    .post_commit_offset = (uintptr_t)&&commit - (uintptr_t)&&start,
    .abort_ip = (uintptr_t)&&abort,
};

// 3. 执行临界区
retry:
    rseq_ptr->rseq_cs = (uintptr_t)&cs;
start:
    cpu = rseq_ptr->cpu_id;
    // 使用 cpu 访问 per-CPU 数据
    per_cpu_data[cpu].counter++;
commit:
    rseq_ptr->rseq_cs = 0;
    goto done;

abort:
    // 签名（必须紧挨在 abort 标签前）
    .int RSEQ_SIG
    rseq_ptr->rseq_cs = 0;
    goto retry;

done:
    // 操作完成
```

## 9. References

- [Linux rseq(2) man page](https://man7.org/linux/man-pages/man2/rseq.2.html)
- [LWN: Restartable sequences](https://lwn.net/Articles/697979/)
- Linux 6.6.21 kernel/rseq.c
