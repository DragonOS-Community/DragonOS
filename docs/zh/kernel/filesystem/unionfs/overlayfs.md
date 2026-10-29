# overlayfs

OverlayFs是目前使用最多的联合文件系统，原理简单方便使用，主要用于容器中
在 Docker 中，OverlayFS 是默认的存储驱动之一。Docker 为每个容器创建一个独立的上层目录，而所有容器共享同一个下层镜像文件。这样的设计使得容器之间的资源共享更加高效，同时减少了存储需求。
## 架构设计
overlayfs主要有两个层，以及一个虚拟的合并层
- Lower Layer（下层）：通常是 只读 文件系统。可以包含多层。
- Upper Layer（上层）：为 可写层，所有的写操作都会在这一层上进行。
- Merged Layer（合并层）：上层和下层的逻辑视图合并后，向用户呈现的最终文件系统。


## 工作原理
- 读取操作：
    -  OverlayFS 会优先从 Upper Layer 读取文件。如果文件不存在于上层，则读取 Lower Layer 中的内容。
- 写入操作：
    - 如果一个文件位于 Lower Layer 中，并尝试写入该文件，系统会将其 copy-up 到 Upper Layer 并在上层写入。如果文件已经存在于 Upper Layer，则直接在该层写入。
- 删除操作：
    - 当删除文件时，OverlayFS 会在上层创建一个标记为 whiteout 的条目，这会隐藏下层的文件。

## Copy-up
- 写时拷贝
当一个文件从 下层 被修改时，它会被复制到 上层（称为 copy-up）。之后的所有修改都会发生在上层的文件副本上。


## 实现逻辑
通过构建ovlInode来实现indexnode这个trait来代表上层或者下层的inode，具体的有关文件文件夹的操作都在