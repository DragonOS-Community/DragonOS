:::{note}
**AI Translation Notice**

This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

- Source document: kernel/ipc/ipc_namespace.md

- Translation time: 2025-09-24 08:16:12

- Translation model: `hunyuan-turbos-latest`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# IPC Namespace

:::{note}

Author: longjin <longjin@dragonos.org>

:::

This page describes the current support status and future plans for IPC namespaces in DragonOS. The goal is to expose semantics consistent with Linux to users while gradually improving upon DragonOS's existing framework.

## Supported Features
- Integration of IpcNamespace object with NsProxy:
  - Added `IpcNamespace` and connected to `NsProxy`, allowing each task to access its associated IPC namespace via `nsproxy.ipc_ns`.
  - Namespace creation/inheritance follows `clone/unshare` semantics:
    - Inherits the parent namespace when `CLONE_NEWIPC` is not included;
    - Creates an independent IPC namespace when `CLONE_NEWIPC` is included;
    - Mutually exclusive with `CLONE_SYSVSEM`, with behavior consistent with Linux.

- SysV SHM (Shared Memory) isolated by namespace:
  - Refactored the original global `SHM_MANAGER` into per-ns `ShmManager`, with all `shmget/shmat/shmdt/shmctl` taking effect under `current.nsproxy.ipc_ns`.
  - `shmat`/`shmdt`: VMA records `ShmId`, precisely maintaining `map_count` during unmapping; after `IPC_RMID`, physical reclamation is completed when `SHM_DEST && map_count==0`.
  - Basic semantics and error codes aligned: `IPC_CREAT|IPC_EXCL`, `ENOENT`, rejection of `SHM_HUGETLB`, etc.

- Basic test cases: (in `test_ipc_ns_shm.rs`)

  - After `unshare(CLONE_NEWIPC)`, keys in parent/child namespaces are not visible;
  - Same keys across namespaces do not conflict;
  - After `IPC_RMID`, the same key can be recreated;
  - Outputs PASS/FAIL and summary results.

## Not Yet Implemented / Planned
- `/proc/[pid]/ns/ipc` and `setns`:
  - Temporarily postponed, with only planning for read-only placeholders and the simplest `setns` path; permission validation and switching sequencing will be added in subsequent versions.

- Other SysV IPC subsystems:
  - `msg/sem` framework not yet incorporated; UNDO lists for `sem` and coordination with `unshare/setns` need to be implemented simultaneously when introduced.

- POSIX mqueue:
  - Per-ns mqueuefs kernel mounting, quotas, and sysctl not yet provided.

- Permissions and quotas:
  - `ipcperms()`, `ns_capable(user_ns, CAP_IPC_OWNER)`;
  - ucounts/RLIMIT and per-ns sysctl such as `/proc/sys/kernel/shm*`.

## Compatibility and Notes
- At this stage, only SysV SHM provides namespace isolation; other IPC types still operate under global semantics.
- Code evolves in a modular manner: when adding `msg/sem/mqueue` later, stability and consistency of user-side semantics will be maintained.

## References
- Code locations:
  - `kernel/src/process/namespace/ipc_namespace.rs`
  - `kernel/src/process/namespace/nsproxy.rs`
  - `kernel/src/ipc/syscall/` within `sys_shm*`
  - `kernel/src/mm/ucontext.rs` (VMA and SHM count maintenance)
