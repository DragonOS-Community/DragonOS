.. note:: AI Translation Notice

   This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

   - Source document: kernel/filesystem/vfs/index.rst

   - Translation time: 2025-06-29 09:05:52

   - Translation model: `Qwen/Qwen3-8B`


   Please report issues via `Community Channel <https://github.com/DragonOS-Community/DragonOS/issues>`_

VFS Virtual File System
====================================

In DragonOS, VFS acts as an adapter, hiding the differences between specific file systems and providing a unified abstract interface for file operations.

VFS is the core of DragonOS's file system, offering a set of unified file system interfaces that allow DragonOS to support various different file systems. The main functions of VFS include:

- Providing a unified file system interface
- Providing mount and unmount mechanisms for file systems (MountFS)
- Providing file abstraction (File)
- Providing file system abstraction (FileSystem)
- Providing IndexNode abstraction
- Providing caching and synchronization mechanisms for file systems (not yet implemented)
- Supporting the mounting of disk devices into the file system (currently supports EXT4 and vfat types of virtio disks)

.. toctree::
   :maxdepth: 1
   :caption: Directory

   design
   api
   mountable_fs
