# IPC Namespace

:::{note}

Author: longjin <longjin@dragonos.org>
 
:::

本页描述 DragonOS 对 IPC 命名空间（IPC namespace）的当前支持状态与后续计划。目标是对用户暴露与 Linux 一致的语义，并在 DragonOS 现有框架上逐步完善。

## 已支持功能
- IpcNamespace 对象与 NsProxy 集成：
  - 新增 `IpcNamespace` 并接入 `NsProxy`，每个任务通过 `nsproxy.ipc_ns` 访问所属 IPC 命名空间。
  - 命名空间的创建/继承遵循 `clone/unshare` 语义：
    - 未包含 `CLONE_NEWIPC` 时继承父命名空间；
    - 包含 `CLONE_NEWIPC` 时创建独立 IPC 命名空间；
    - 与 `CLONE_SYSVSEM` 互斥，行为与 Linux 一致。

- SysV SHM（共享内存）按命名空间隔离：
  - 将原全局 `SHM_MANAGER` 重构为 per-ns `ShmManager`，所有 `shmget/shmat/shmdt/shmctl` 均在 `current.nsproxy.ipc_ns` 下生效。
  - `shmat`/`shmdt`：VMA 记录 `ShmId`，解除映射时精确维护 `map_count`；`IPC_RMID` 后当 `SHM_DEST && map_count==0` 即完成物理回收。
  - 基本语义与错误码对齐：`IPC_CREAT|IPC_EXCL`、`ENOENT`、拒绝 `SHM_HUGETLB` 等。

- 基础测试用例：（在 `test_ipc_ns_shm.rs` 中）

  - `unshare(CLONE_NEWIPC)` 后父/子命名空间的 key 不可见；
  - 跨命名空间相同 key 不冲突；
  - `IPC_RMID` 后可重新创建同 key；
  - 输出 PASS/FAIL 与汇总结果。

## 暂未实现/计划中
- `/proc/[pid]/ns/ipc` 与 `setns`：
  - 暂缓，仅规划只读占位与最简 `setns` 路径；后续版本补齐权限校验与切换时序。

- SysV IPC 其它子系统：
  - `msg/sem` 框架尚未纳入；`sem` 的 UNDO 列表与 `unshare/setns` 的协同需在引入时同步实现。

- POSIX mqueue：
  - 尚未提供 per-ns mqueuefs 内核挂载、限额与 sysctl。

- 权限与配额：
  - `ipcperms()`、`ns_capable(user_ns, CAP_IPC_OWNER)`；
  - ucounts/RLIMIT 与 `/proc/sys/kernel/shm*` 等 per-ns sysctl。

## 兼容性与注意事项
- 当前阶段仅对 SysV SHM 提供命名空间隔离；其它 IPC 类型仍按全局语义工作。
- 代码按模块化方式演进：后续加入 `msg/sem/mqueue` 时，保持对用户侧语义的稳定与一致。

## 参考
- 代码位置：
  - `kernel/src/process/namespace/ipc_namespace.rs`
  - `kernel/src/process/namespace/nsproxy.rs`
  - `kernel/src/ipc/syscall/` 内的 `sys_shm*`
  - `kernel/src/mm/ucontext.rs`（VMA 与 SHM 计数维护）


