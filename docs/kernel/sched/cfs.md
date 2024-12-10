# 完全公平调度器相关的api

&emsp;&emsp; CFS（Completely Fair Scheduler），顾名思义，完全公平调度器。CFS作为主线调度器之一，也是最典型的O(1)调度器之一

## 结构体介绍

- ``CompletelyFairScheduler``
&emsp;&emsp; ``CompletelyFairScheduler``实现了``Scheduler``trait，他是完全调度算法逻辑的主要实施者。

- ``FairSchedEntity``
	- **重要字段**
		- ``cfs_rq``: 它指向了自己所在的完全公平调度队列。
		- ``my_cfs_rq``: 为一个``Option``变量，当该实体作为一个单独进程时，这个值为``None``，但是若这个实体为一个组，那这个变量必需为这个组内的私有调度队列。这个``cfs_rq``还可以继续往下深入，就构成了上述的树型结构。
		- ``pcb``: 它指向了当前实体对应的``PCB``，同样，若当前实体为一个组，则这个``Weak``指针不指向任何值。

&emsp;&emsp;``FairSchedEntity``是完全公平调度器中最重要的结构体，他代表一个实体单位，它不止表示一个进程，它还可以是一个组或者一个用户，但是它在cfs队列中所表示的就单单是一个调度实体。这样的设计可以为上层提供更多的思路，比如上层可以把不同的进程归纳到一个调度实体从而实现组调度等功能而不需要改变调度算法。

&emsp;&emsp;在cfs中，整体的结构是**一棵树**，每一个调度实体作为``cfs_rq``中的一个节点，若该调度实体不是单个进程（它可能是一个进程组），则在该调度实体中还需要维护一个自己的``cfs_rq``，这样的嵌套展开后，每一个叶子节点就是一个单独的进程。需要理解这样一棵树，**在后续文档中会以这棵树为核心讲解**。
&emsp;&emsp;该结构体具体的字段意义请查阅源代码。这里提及几个重要的字段：


- ``CfsRunQueue``
&emsp;&emsp;``CfsRunQueue``完全公平调度算法中管理``FairSchedEntity``的队列，它可以挂在总的``CpuRunQueue``下，也可以作为子节点挂在``FairSchedEntity``上，详见上文``FairSchedEntity``。

	- **重要字段**
		- ``entities``: 存储调度实体的红黑树
		- ``current``: 当前正在运行的实体

