# VFS Virtual File System

In DragonOS, VFS acts as an adapter that masks the differences between specific file systems, providing a unified abstract interface for file operations externally.

VFS is the core of DragonOS's file system. It provides a set of unified file system interfaces, enabling DragonOS to support multiple different file systems. The main functions of VFS include:

- Providing a unified file system interface
- Providing file system mounting and unmounting mechanisms (MountFS)
- Providing mount propagation mechanisms (Shared/Private/Slave/Unbindable)
- Providing file abstraction (File)
- Providing file system abstraction (FileSystem)
- Providing IndexNode abstraction
- Providing file system caching and synchronization mechanisms (not yet implemented)
- Supporting the mounting of hard disk devices onto the file system (currently supporting EXT4 and vfat types of virtio disks)
