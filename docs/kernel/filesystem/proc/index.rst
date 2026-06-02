====================================
ProcFS
====================================

ProcFS 用于向用户态导出内核中的进程、挂载、命名空间等运行时信息。

当前目录主要介绍 DragonOS 中与挂载导出相关的 proc 接口实现，包括：

- `/proc/mounts`
- `/proc/[pid]/mounts`
- `/proc/[pid]/mountinfo`
- `/proc/[pid]/mountstats`

.. toctree::
   :maxdepth: 1

   mounts
