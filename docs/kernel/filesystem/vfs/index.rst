
VFS虚拟文件系统
====================================

在DragonOS中，VFS作为适配器，遮住了具体文件系统之间的差异，对外提供统一的文件操作接口抽象。

VFS是DragonOS文件系统的核心，它提供了一套统一的文件系统接口，使得DragonOS可以支持多种不同的文件系统。VFS的主要功能包括：

- 提供统一的文件系统接口
- 提供文件系统的挂载和卸载机制（MountFS）
- 提供文件抽象（File）
- 提供文件系统的抽象（FileSystem）
- 提供IndexNode抽象
- 提供文件系统的缓存、同步机制（尚未实现）
- 支持将硬盘设备挂载到文件系统上（目前支持EXT4和vfat类型的virtio硬盘）


.. toctree::
   :maxdepth: 1
   :caption: 目录

   design
   api
   mountable_fs

