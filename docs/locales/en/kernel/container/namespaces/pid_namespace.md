:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/container/namespaces/pid_namespace.md

- Translation time: 2025-05-19 01:41:31

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Process Namespace
:::{note} Author: Cao Fengyi 1553389239@qq.com

October 30, 2024
:::

`pid_namespace` is a type of namespace in the kernel that is used to achieve process isolation. It allows processes running in different namespaces to have independent views of process IDs (PIDs).

## Underlying Architecture

pcb -> nsproxy -> pid_namespace
- `pid_namespace` contains an independent set of process allocators and an orphan process reaper, which independently manages PIDs within the namespace.
- Detailed information about processes is stored in the proc file system. The information corresponding to a specific PID is located within the `pid_namespace`, recording information related to the `pid_namespace`.
- The limitations imposed by `pid_namespace` are controlled and managed by `ucount`.

## System Call Interface

- `clone`
    - `CLONE_NEWPID` is used to create a new PID namespace. When this flag is used, the child process will run in the new PID namespace, with the process ID starting from 1.
- `unshare`
    - After calling `unshare()` with the `CLONE_NEWPID` flag, all subsequent child processes will run within the new namespace.
- `getpid`
    - Calling `getpid()` within a namespace returns the process ID of the process within the current PID namespace.
