.. note:: AI Translation Notice

   This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

   - Source document: kernel/filesystem/vfs/index.rst

   - Translation time: 2025-05-19 01:41:14

   - Translation model: `Qwen/Qwen3-8B`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

VFS Virtual File System
====================================

In DragonOS, VFS acts as an adapter, hiding the differences between specific file systems and providing a unified file operation interface abstraction to the outside.

VFS is the core of the file system in DragonOS. It provides a set of unified file system interfaces, enabling DragonOS to support various different file systems. The main functions of VFS include:

- Providing a unified file system interface
- Providing mount and unmount mechanisms for file systems (MountFS)
- Providing file abstraction (File)
- Providing file system abstraction (FileSystem)
- Providing IndexNode abstraction
- Providing caching and synchronization mechanisms for file systems (not yet implemented)

.. toctree::
   :maxdepth: 1
   :caption: Directory

   design
   api
