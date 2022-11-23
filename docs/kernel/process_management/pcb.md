# PCB 进程控制块

PCB的全称为process control block, 它是每个进程/线程的核心控制结构。定义于`kernel/src/process/proc-types.h`中。

## PCB详解

Todo:

## 与PCB的管理相关的API

### 根据pid寻找pcb

**process_find_pcb_by_pid**

该API提供了根据pid寻找pcb的功能，定义在`kernel/src/process/process.h`中。

当找到目标的pcb时，返回对应的pcb，否则返回NULL。

#### 参数

**pid**
    进程id

#### 返回值

**struct process_control_block**
    目标pcb