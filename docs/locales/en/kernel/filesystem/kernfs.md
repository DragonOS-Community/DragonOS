:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/filesystem/kernfs.md

- Translation time: 2025-05-19 01:41:15

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# KernFS

:::{note}

Maintainer:
- Long Jin <longjin@dragonos.org>
:::

## 1. Introduction
&emsp;&emsp;KernFS is a pseudo file system that acts as a container for other kernel file systems, providing a file interface to users. Its core functionality is that when files in KernFS are read/written or trigger callback points, the predefined callback functions will be invoked, triggering operations on other kernel file systems.

&emsp;&emsp;This design decouples the basic operations of SysFS and file systems. KernFS serves as the carrier of SysFS, allowing SysFS to focus more on the management of KObjects, resulting in more elegant code.

&emsp;&emsp;In the future, the kernel subsystem of DragonOS or other kernel file systems can use KernFS as a carrier for file system operations, decoupling the system management logic from specific file system operations.

## 2. Usage

&emsp;&emsp;Taking SysFS as an example, a new KernFS instance is created as the file system interface for SysFS, and then it is mounted under the directory `/sys`. Then, sysfs implements the upper-layer logic to manage KObjects. Each upper-layer KObject must include a KernFSInode. By setting the PrivateData of KernFSInode, KernFS can retrieve the corresponding KObject or sysfs attribute based on the Inode. Furthermore, when creating a KernFSInode, different callbacks are passed to the specific Inode, enabling "different Inodes to trigger different callback behaviors when read or written."

&emsp;&emsp;When a callback occurs, KernFS passes the callback information and private information to the callback function, allowing the callback function to retrieve the corresponding KObject or sysfs attribute based on the input information, thus achieving the high-level functionality provided by sysfs.

&emsp;&emsp;From the above description, we can see that KernFS achieves the purpose of "decoupling specific file operations from high-level management logic" by storing the callback functions and callback information of the upper-layer file systems.
