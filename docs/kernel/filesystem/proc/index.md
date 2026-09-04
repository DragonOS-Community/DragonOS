# ProcFS

ProcFS is used to export runtime information such as processes, mounts, and namespaces from the kernel to user space.

The current directory primarily introduces the implementation of proc interfaces related to mount exports in DragonOS, including:

- `/proc/mounts`
- `/proc/[pid]/mounts`
- `/proc/[pid]/mountinfo`
- `/proc/[pid]/mountstats`
