# 进程名称空间
:::{note} 本文作者：操丰毅 1553389239@qq.com

2024年10月30日 :::
pid_namespace 是内核中的一种名称空间，用于实现进程隔离，允许在不同的名称空间中运行的进程有独立的pid试图

## 底层架构

pcb -> nsproxy -> pid_namespace
- pid_namespace 内有独立的一套进程分配器，以及孤儿进程回收器，独立管理内部的pid
- 不同进程的详细信息都存放在proc文件系统中，里面的找到对应的pid号里面的信息都在pid中，记录的是pid_namespace中的信息
- pid_namespace等限制由ucount来控制管理

## 系统调用接口

- clone
    - CLONE_NEWPID用于创建一个新的 PID 命名空间。使用这个标志时，子进程将在新的 PID 命名空间内运行，进程 ID 从 1 开始。
- unshare 
    - 使用 CLONE_NEWPID 标志调用 unshare() 后，后续创建的所有子进程都将在新的命名空间中运行。
- getpid
    - 在命名空间中调用 getpid() 会返回进程在当前 PID 命名空间中的进程 ID