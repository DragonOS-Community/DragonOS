# KernFS

:::{note}

Maintainer:
- 龙进 <longjin@dragonos.org>
:::

## 1. 简介
&emsp;&emsp;KernFS是一个伪文件系统，它充当其它内核文件系统的容器，面向用户提供文件接口。其核心功能就是，当kernfs的文件被读/写或者触发回调点的时候，将会对预设的回调函数进行调用，触发其它内核文件系统的操作。

&emsp;&emsp;这种设计使得SysFS和文件系统的基本操作解耦，KernFS作为SysFS的承载物，使得SysFS能更专注于KObject的管理，让代码更加优雅。

&emsp;&emsp;在未来，DragonOS的内核子系统，或者其它的内核文件系统，可以使用KernFS作为文件系统操作的承载物，让系统管理的逻辑与具体的文件系统操作解除耦合。

## 2. 使用方法

&emsp;&emsp;以SysFS为例，新创建一个KernFS实例，作为SysFS的文件系统接口，然后挂载到`/sys`目录下。接着sysfs实现上层逻辑，管理KObject，每个上层的Kobject里面都需要包含KernFSInode。并且通过设置KernFSInode的PrivateData，使得KernFS能够根据Inode获取到其指向的KObject或者sysfs的attribute。并且在创建KernFSInode的时候，为具体的Inode传入不同的callback，以此实现“不同的Inode在读写时能够触发不同的回调行为”。

&emsp;&emsp;当发生回调时，KernFS会把回调信息、私有信息传入到回调函数中，让回调函数能够根据传入的信息，获取到对应的KObject或者sysfs的attribute，从而实现sysfs提供的高层功能。

&emsp;&emsp;从上述描述我们能够看出：KernFS就是通过存储上层文件系统的回调函数、回调信息，来实现“把具体文件操作与高层管理逻辑进行解耦”的目的。
