:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/syscall/sys_capget_capset.md

- Translation time: 2025-09-25 09:18:48

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Design Documentation for sys_capget / sys_capset

This document briefly introduces the design and implementation key points of sys_capget and sys_capset in DragonOS, covering version negotiation, user-space data structures, capability bitset rules, and call flows.

Source Code:
- kernel/src/process/syscall/sys_cap_get_set.rs
- kernel/src/process/cred.rs

## Overview

- DragonOS aligns with Linux's capability interface, supporting user-space reading or setting process capability sets via capget/capset.
- Capability sets include:
  - cap_effective (pE): The capabilities currently in effect for the process
  - cap_permitted (pP): The upper limit of capabilities granted to the process
  - cap_inheritable (pI): Capabilities that can be inherited by child processes
  - cap_bset: Bounding set, limiting the upper bound of obtainable capabilities (used only for rule constraints, not directly read/written in this interface)
  - cap_ambient: Ambient set (not modified by capset)
- Capability bit width: DragonOS uses 64-bit storage but currently only supports the lower 41 bits (CAP_FULL_SET = (1<<41)-1), with higher bits truncated.

## User-Space Data Structures and Versions

Aligned with Linux's user-space structures:

```c
// header: cap_user_header_t
struct CapUserHeader {
    uint32_t version; // 版本号
    int32_t  pid;     // 目标进程: 0=当前进程，其他=指定pid
};

// data: cap_user_data_t 数组元素
struct CapUserData {
    uint32_t effective;
    uint32_t permitted;
    uint32_t inheritable;
}
```

- Version constants:
  - _LINUX_CAPABILITY_VERSION_1 = 0x19980330
  - _LINUX_CAPABILITY_VERSION_2 = 0x20071026 (deprecated)
  - _LINUX_CAPABILITY_VERSION_3 = 0x20080522
- Kernel-supported version in DragonOS: _KERNEL_CAPABILITY_VERSION = v3
- Number of u32 groups copied per version:
  - v1: 1 group (lower 32 bits only)
  - v2/v3: 2 groups (lower 32 bits + upper 32 bits)

Aggregation/Splitting Rules:
- capset: Aggregates CapUserData[0..tocopy) from user input into a u64 (truncated to 41 bits at higher positions)
- capget: Returns the number of u32 groups corresponding to the requested version (v1:1 group; v2/v3:2 groups) based on the request, also returning 0 when data==NULL.

## Version Negotiation and Probe Behavior

- capget:
  - If version is unknown: Writes back header.version as the kernel-supported version (v3) and returns:
    - If data==NULL: Returns 0 (for probing)
    - If data!=NULL: Returns EINVAL
  - If version is valid: Returns the number of u32 groups corresponding to the requested version (v1:1 group; v2/v3:2 groups), also returning 0 when data==NULL.
- capset:
  - If version is unknown: Directly returns EINVAL (does not take on probing responsibility), more consistent with Linux.
  - data cannot be empty (NULL returns EFAULT).

## Target Process Selection and pid Semantics

- capget:
  - pid < 0: EINVAL
  - pid == 0: Uses the current process
  - pid != 0: Looks up the target process (returns ESRCH if not found)
- capset:
  - pid < 0: EPERM (negative pid targets not allowed)
  - pid == 0 or pid == current process pid: Allowed
  - pid != current process pid: EPERM (only self-modification allowed)

## Capability Set Rules (capset)

Let:
- pE_old = old effective
- pP_old = old permitted
- pI_old = old inheritable
- bset   = bounding set
- pE_new, pP_new, pI_new derived from user data (already truncated to 41-bit mask)

Constraints:
1) pE_new ⊆ pP_new  
   If any bit in pE_new is not in pP_new: EPERM

2) pP_new ⊆ pP_old (not allowed to elevate permitted)  
   If pP_new contains any bits not in pP_old: EPERM

3) pI_new limitation (aligned with Linux's CAP_SETPCAP and bset constraints)
   - If the current process has CAP_SETPCAP_BIT (in the pE_old effective set):
     pI_new ⊆ (pI_old ∪ pP_old) ∩ bset  
     If exceeded: EPERM
   - If not:
     pI_new ⊆ (pI_old ∪ pP_old) and pI_new ⊆ (pI_old ∪ bset)  
     Any exceedance: EPERM

Note:
- Ambient capabilities are not modified by capset and remain unchanged.
- By cloning the old cred, updating pE/pP/pI, and then atomically replacing it in the PCB (pcb.set_cred).

## Flowchart

Main flow of capget:

```
[读取 header(version,pid)]
        |
   [版本合法?]
      /     \
    否       是
    |         |
[写回 header.version=v3]     [pid 选择]
        |                     |-- pid<0 -> EINVAL
   [data==NULL?]              |-- pid==0 -> 当前进程 cred
      /     \                 |-- pid!=0 -> 查找目标任务
    是       否               |              |- 未找到 -> ESRCH
    |         |               |              |- 找到 -> 目标 cred
  返回 0     EINVAL           |
                              [拆分 e/p/i 为低/高 32 位]
                              [data==NULL?]
                                /       \
                              是         否
                               |          |
                             返回 0     写回用户缓冲区，返回 0
```

Main flow of capset:

```
[读取 header(version,pid)]
        |
   [版本合法?]
      /     \
    否       是
    |         |
  EINVAL   [data==NULL?]
              /      \
            是        否
             |         |
           EFAULT    [pid 检查]
                      |- pid<0 -> EPERM
                      |- pid!=self -> EPERM
                      |- pid==self -> [读取用户数据并聚合 pE/pP/pI]
                                      [规则1: pE_new ⊆ pP_new?]  否 -> EPERM
                                      [规则2: pP_new ⊆ pP_old?] 否 -> EPERM
                                      [规则3: pI_new 受 CAP_SETPCAP/bset 限制?] 否 -> EPERM
                                      [克隆 cred 更新 pE/pP/pI]
                                      [pcb.set_cred 原子替换]
                                      返回 0
```

## Capability Bit Width and Masks

Apply masks to e/p/i during aggregation:
- mask = CAPFlags::CAP_FULL_SET.bits() = (1<<41)-1
- Higher bits are truncated to ensure cross-version compatibility and consistency with the current implementation.

## Design Trade-offs and Alignment

- capget supports "probe" semantics for unknown versions: writes back the supported version and returns 0 when data==NULL.
- capset does not take on probing: unknown versions directly return EINVAL, more closely aligned with Linux behavior.
- pid constraints are stricter: capset only allows modification of the current process to avoid cross-process permission modifications.
- Rules follow the Linux capability model: not allowed to elevate permitted; effective must be limited by permitted; inheritable is constrained by CAP_SETPCAP and bset.

## Future Work

- Improve more interfaces for ambient capabilities and bounding set (currently ambient is not modified in capset).
- Introduce more complete capability bit definitions and permission check interfaces.
- Align documentation and test cases with more boundary conditions (such as the impact of user namespaces).
