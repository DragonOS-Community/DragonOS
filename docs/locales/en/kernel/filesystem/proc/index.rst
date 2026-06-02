.. note:: AI Translation Notice

   This document was automatically translated by `hunyuan-turbos-latest` model, for reference only.

   - Source document: kernel/filesystem/proc/index.rst

   - Translation time: 2026-06-02 17:27:12

   - Translation model: `hunyuan-turbos-latest`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

====================================
ProcFS
====================================

ProcFS is used to export runtime information such as processes, mounts, and namespaces from the kernel to user space.

The current directory primarily introduces the implementation of proc interfaces related to mount exports in DragonOS, including:

- `/proc/mounts`
- `/proc/[pid]/mounts`
- `/proc/[pid]/mountinfo`
- `/proc/[pid]/mountstats`

.. toctree::
   :maxdepth: 1

   mounts
