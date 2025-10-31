# sys_capget / sys_capset 设计说明

本文简要介绍 DragonOS 中 sys_capget 和 sys_capset 的设计与实现要点，覆盖版本协商、用户态数据结构、能力位集规则、以及调用流程。

来源代码：
- kernel/src/process/syscall/sys_cap_get_set.rs
- kernel/src/process/cred.rs

## 概述

- DragonOS 对齐 Linux 的 capability 接口，支持用户态通过 capget/capset 读取或设置进程的能力集。
- 能力集包括：
  - cap_effective (pE)：当前进程实际生效的能力
  - cap_permitted (pP)：进程被赋予的能力上限
  - cap_inheritable (pI)：可被子进程继承的能力
  - cap_bset：bounding set，限制可获得能力的上界（仅用于规则约束，不在本接口直接读写）
  - cap_ambient：ambient set（不由 capset 修改）
- 能力位宽：DragonOS 使用 64 位存储，但当前仅支持低 41 位（CAP_FULL_SET = (1<<41)-1），高位截断。

## 用户态数据结构与版本

与 Linux 的用户态结构对齐：

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

- 版本常量：
  - _LINUX_CAPABILITY_VERSION_1 = 0x19980330
  - _LINUX_CAPABILITY_VERSION_2 = 0x20071026（已废弃）
  - _LINUX_CAPABILITY_VERSION_3 = 0x20080522
- DragonOS 内核支持版本：_KERNEL_CAPABILITY_VERSION = v3
- 每版本拷贝的 u32 数量：
  - v1: 1 组（仅低 32 位）
  - v2/v3: 2 组（低 32 位 + 高 32 位）

聚合/拆分规则：
- capset: 从用户传入的 CapUserData[0..tocopy) 聚合为 u64（高位截断到 41 位）
- capget: 根据请求版本返回 1 组或 2 组 u32（高位通过右移 32 获得）

## 版本协商与探测行为

- capget:
  - 若版本未知：写回 header.version 为内核支持版本（v3），并返回：
    - 若 data==NULL：返回 0（用于探测）
    - 若 data!=NULL：返回 EINVAL
  - 若版本合法：返回请求版本对应数量的 u32 组（v1:1组；v2/v3:2组），data==NULL 时也返回 0。
- capset:
  - 若版本未知：直接返回 EINVAL（不承担探测职责），与 Linux 更一致。
  - data 不能为空（NULL 返回 EFAULT）。

## 目标进程选择与 pid 语义

- capget:
  - pid < 0：EINVAL
  - pid == 0：使用当前进程
  - pid != 0：查找目标任务（找不到返回 ESRCH）
- capset:
  - pid < 0：EPERM（不允许负 pid 目标）
  - pid == 0 或 pid == 当前进程 pid：允许
  - pid != 当前进程 pid：EPERM（仅允许修改自身）

## 能力集规则（capset）

设：
- pE_old = 旧 effective
- pP_old = 旧 permitted
- pI_old = 旧 inheritable
- bset   = bounding set
- pE_new, pP_new, pI_new 由用户数据聚合得出（已按 41 位掩码截断）

约束：
1) pE_new ⊆ pP_new  
   若存在 pE_new 中的位不在 pP_new：EPERM

2) pP_new ⊆ pP_old（不允许提升 permitted）  
   若 pP_new 中存在不属于 pP_old 的位：EPERM

3) pI_new 限幅（对齐 Linux 的 CAP_SETPCAP 与 bset 约束）
   - 如果当前进程具有 CAP_SETPCAP_BIT（在 pE_old 生效集合中）：
     pI_new ⊆ (pI_old ∪ pP_old) ∩ bset  
     若超出：EPERM
   - 如果不具有：
     pI_new ⊆ (pI_old ∪ pP_old) 且 pI_new ⊆ (pI_old ∪ bset)  
     任一超出：EPERM

注意：
- ambient 能力不由 capset 修改，保持不变。
- 通过克隆旧 cred，更新 pE/pP/pI 后，原子替换到 PCB（pcb.set_cred）。

## 流程图

capget 主要流程：

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

capset 主要流程：

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

## 能力位宽与掩码

聚合时对 e/p/i 应用掩码：
- mask = CAPFlags::CAP_FULL_SET.bits() = (1<<41)-1
- 高位被截断，保证跨版本兼容性与当前实现的一致性。

## 设计取舍与对齐

- capget 对未知版本支持“探测”语义：写回支持版本并在 data==NULL 时返回 0。
- capset 不承担探测：未知版本直接 EINVAL，更贴近 Linux 行为。
- pid 约束更严格：capset 仅允许修改当前进程，避免跨进程权限修改。
- 规则遵循 Linux 能力模型：不允许提升 permitted；effective 必须受限于 permitted；inheritable 受 CAP_SETPCAP 与 bset 限制。

## 未来工作

- 完善 ambient 能力与 bounding set 的更多接口（当前不在 capset 中修改 ambient）。
- 引入更完整的能力位定义与权限检查接口。
- 文档与测试用例对齐更多边界条件（如用户命名空间影响）。
