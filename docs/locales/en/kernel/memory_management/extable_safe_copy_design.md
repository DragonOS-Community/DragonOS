:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/memory_management/extable_safe_copy_design.md

- Translation time: 2025-11-18 13:03:29

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Design of Secure Memory Copy Scheme Based on Exception Table

:::{note}

Author: Long Jin <longjin@dragonos.org>

:::

## Overview

This document describes the core design concept of a secure memory copy scheme in DragonOS based on the Exception Table mechanism. This solution addresses the issue of safely accessing user-space memory in system call contexts, preventing kernel panics caused by accessing invalid user addresses.

## Design Background and Motivation

### Problem Definition

During system call processing, the kernel needs to access pointers passed from user space (such as path strings, parameter structures, etc.). These accesses may fail due to:

1. **Unmapped address**: The user-provided address has no corresponding VMA (Virtual Memory Area)
2. **Insufficient permissions**: The page exists but lacks required permissions
3. **Malicious input**: The user intentionally provides illegal addresses

### Limitations of Traditional Solutions

**TOCTTOU issues with pre-checking solutions:**
- Addresses may be valid during check but modified by other threads when used
- Race condition windows exist

**Dilemma of direct access:**
- Cannot distinguish between "normal page fault" and "illegal access"
- Page fault handlers cannot determine whether it's a kernel bug or user error

## Principles of Exception Table Mechanism

### Core Idea

The Exception Table mechanism achieves secure user-space access through **compile-time marking + runtime lookup**:

1. **Compile-time**: Generate exception table entries at instructions that may trigger page faults
2. **Runtime**: When a page fault occurs, search the exception table and jump to fix-up code
3. **Zero overhead**: No performance loss on normal paths

### Architectural Diagram

```
┌─────────────────────────────────────────────────────────────┐
│                      系统调用执行流程                          │
├─────────────────────────────────────────────────────────────┤
│                                                             │
│   用户空间                  内核空间                           │
│  ┌──────┐         ┌──────────────────────────────┐          │
│  │0x1000│         │ 1. 系统调用入口                │          │
│  │(未映射)─────────→ 2. 拷贝用户数据(带标记)         │          │
│  └──────┘         │    ├─ 正常完成 ──→ 返回成功     │          │
│                   │    └─ 触发#PF                 │          │
│                   │         ↓                    │          │
│                   │ 3. 页错误处理器                │          │
│                   │    ├─ 查找异常表               │          │
│                   │    └─ 找到修复代码地址          │          │
│                   │         ↓                    │          │
│                   │ 4. 修改指令指针(RIP)           │          │
│                   │         ↓                    │          │
│                   │ 5. 执行修复代码                │          │
│                   │    └─ 设置错误码(-1)           │          │
│                   │         ↓                    │          │
│                   │ 6. 返回EFAULT给用户            │          │
│                   └──────────────────────────────┘          │
└─────────────────────────────────────────────────────────────┘
```

### Core Data Structures

**Exception Table Entry (8-byte aligned):**
```
┌─────────────────┬──────────────────┐
│  指令相对偏移     │  修复代码相对偏移   │
│    (4 bytes)    │    (4 bytes)     │
└─────────────────┴──────────────────┘
```

**Design Highlights:**
- Uses relative offsets to support ASLR (Address Space Layout Randomization)
- 8-byte alignment improves cache performance
- Stored in read-only segments to prevent tampering

### Workflow

```
编译期:
    源码 ──→ 带标记的指令 ──→ 生成异常表条目 ──→ 链接到内核镜像
              (rep movsb)       (insn→fixup)

运行期:
    执行拷贝 ──→ 触发页错误? ─否→ 正常返回
                     │
                    是
                     ↓
              查找异常表 ──→ 找到? ─否→ 内核panic
                              │
                             是
                              ↓
                    修改RIP到修复代码 ──→ 返回错误码
```

## Typical Execution Scenarios

### Scenario: System Call with Invalid Address

Taking the `open()` system call as an example, demonstrating the operation of the exception table:

```
用户程序: open(0x1000, O_RDONLY)  // 0x1000未映射
         │
         ↓
    ┌────────────────────────────────┐
    │ 1. 进入系统调用                  │
    │    ├─ 解析路径字符串             │
    │    └─ 逐字节拷贝直到'\0'         │
    └────────────────────────────────┘
         │
         ↓
    ┌────────────────────────────────┐
    │ 2. 拷贝第一个字节时触发页错误      │
    │    (地址0x1000不在VMA中)         │
    └────────────────────────────────┘
         │
         ↓
    ┌────────────────────────────────┐
    │ 3. 页错误处理器                  │
    │    ├─ 检测到访问用户地址          │
    │    ├─ 查找异常表                 │
    │    └─ 找到对应的修复代码          │
    └────────────────────────────────┘
         │
         ↓
    ┌────────────────────────────────┐
    │ 4. 修改指令指针到修复代码          │
    │    └─ 设置返回值为错误码          │
    └────────────────────────────────┘
         │
         ↓
    ┌────────────────────────────────┐
    │ 5. 系统调用返回EFAULT            │
    └────────────────────────────────┘
         │
         ↓
用户程序: fd = -1, errno = EFAULT
```

**Key Points:**
- No need for pre-checking address validity
- Page faults are automatically converted to error codes
- Kernel won't panic, user programs receive clear error information

## Usage Scenario Analysis

### ✅ Suitable Scenarios for Exception Table Protection

#### 1. Small Data System Call Parameters

**Characteristics:**
- Small data volume (typically < 4KB)
- Single copy operation
- Unknown data length (e.g., strings)

**Typical Applications:**
- Path strings: `open()`, `stat()`, `execve()`, etc.
- Fixed-size structures: `sigaction`, `timespec`, `stat`, etc.
- Small arrays: `iovec[]`, `pollfd[]`, etc.

**Advantages:**
- **Avoids TOCTTOU races**: No pre-checking needed
- **High robustness**: User errors won't cause kernel panics
- **Acceptable performance**: Small data volume, minimal impact even with extra copies

#### 2. Scenarios with Uncertain Address Validity

When address validity cannot be verified by other means, the exception table is the safest choice:
- Raw pointers directly provided by users
- Addresses that may be concurrently modified in multi-threaded environments
- Operations requiring atomicity guarantees

### ❌ Unsuitable Scenarios for Exception Table Protection

#### 1. Large Data Transfers

**Anti-pattern: Double buffering in read/write system calls**
```
用户缓冲区 → 内核临时缓冲区 → 用户缓冲区  ❌
```

**Issues:**
- Memory waste: Requires additional kernel buffers
- Double copying: Data is copied twice
- OOM risk: Concurrent large reads/writes may exhaust memory

**Correct Approach: Zero-copy**
- Pre-validate addresses within valid VMAs
- Operate directly on user buffers
- Page faults trigger normal page fault handling (not errors)

#### 2. Addresses Already Validated in VMAs

If addresses have been validated through VMA checks, the exception table is redundant:
- Immediate access after `mmap()`
- DMA buffers
- Shared memory regions

In these scenarios, page faults are **normal page fault handling** (like COW), not errors.

#### 3. Performance-Critical Hot Paths

Avoid frequently calling functions protected by exception tables in loops:
- **Batch processing**: Copy entire arrays at once rather than element-by-element
- **Pre-validation**: Validate addresses outside loops, direct access inside loops

### Decision Matrix

| Scenario Characteristics | Data Volume | Recommended Solution | Key Considerations |
|--------------------------|-------------|----------------------|--------------------|
| Small system call parameters | < 4KB | Exception table protection | Avoid TOCTTOU, improve robustness |
| File I/O | Variable (MB-level) | Zero-copy | Performance priority, avoid double buffering |
| Access after mmap | Any | Direct access | VMA already validated, normal page fault |
| Batch small data | Cumulative KB-level | Batch copy | Reduce system call count |
| String parsing | Unknown | Exception table protection | Byte-by-byte scanning, requires robustness |

## Security Analysis

### Defensive Capabilities

The Exception Table mechanism can defend against:

1. **Null pointer dereferencing**: Returns EFAULT instead of segmentation fault
2. **Kernel address injection**: User-provided kernel addresses are safely rejected
3. **Race attacks**: TOCTTOU windows are eliminated
4. **Out-of-bounds access**: Accesses outside VMAs are captured

### Security Boundaries

The Exception Table **cannot** defend against:

1. **Kernel bugs**: Such as wild pointer dereferencing
2. **Hardware failures**: Physical memory damage
3. **Other exception types**: Only handles page faults

### Multi-layer Defense

The Exception Table is part of defense in depth:

```
┌─────────────────────────────────────┐
│  用户空间权限检查 (SELinux/AppArmor)   │  ← 权限层
├─────────────────────────────────────┤
│  系统调用参数验证                      │  ← 逻辑层
├─────────────────────────────────────┤
│  异常表机制                           │  ← 内存安全层
├─────────────────────────────────────┤
│  硬件页保护 (MMU)                     │  ← 硬件层
└─────────────────────────────────────┘
```

## Implementation Essentials

### Key Technologies

1. **Relative offset encoding**: Supports address randomization (ASLR)
2. **Binary search**: O(log n) time complexity for fast location
3. **Inline assembly**: Precise control over instruction generation and exception table creation
4. **Zero-overhead abstraction**: No performance loss on normal paths

### Architecture Portability

The Exception Table mechanism can be ported to other architectures:

- **x86_64**: Uses `rep movsb` instruction
- **ARM64**: Uses `ldp/stp` instruction sequence
- **RISC-V**: Uses `ld/sd` instruction sequence

The core concept remains unchanged, only assembly syntax needs adjustment.
