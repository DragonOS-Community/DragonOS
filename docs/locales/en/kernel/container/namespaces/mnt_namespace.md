:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/container/namespaces/mnt_namespace.md

- Translation time: 2025-05-19 01:41:19

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Mount Namespace

## Underlying Architecture

pcb -> nsproxy -> mnt_namespace

Each mounted file system has its own independent mount point, which is represented in the data structure as a red-black tree of mounts. Each namespace has its own independent mounts, so mounting and unmounting file systems will not affect others.

## System Call Interface

- clone
    - CLONE_NEWNS is used to create a new MNT namespace. It provides an independent file system mount point.
- unshare
    - After calling unshare() with the CLONE_NEWPID flag, all subsequent child processes will run in the new namespace.
- setns
    - Adds the process to the specified namespace.
- chroot
    - Changes the current process's root directory to the specified path, providing file system isolation.
