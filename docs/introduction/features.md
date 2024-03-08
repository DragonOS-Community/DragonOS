(_genreal_features)=

# DragonOS的功能

## 规范

- [x] 启动引导：Multiboot2

- [x] 接口：posix 2008

## 内核层

### 内存管理

- [x] 页帧分配器
- [x] 小对象分配器
- [x] VMA
- [x] MMIO地址空间自动分配
- [x] 页面映射器
- [x] 硬件抽象层
- [x] 独立的用户地址空间管理机制
- [x] C接口兼容层

### 多核

- [x] 多核引导
- [x] ipi框架

### 进程管理

- [x] 进程创建
- [x] 进程回收
- [x] 内核线程
- [x] fork
- [x] exec
- [x] 进程睡眠（支持高精度睡眠）
- [x] kthread机制
- [x] 可扩展二进制加载器

#### 同步原语

- [x] mutex互斥量
- [x] semaphore信号量
- [x] atomic原子变量
- [x] spinlock自旋锁
- [x] wait_queue等待队列

### 调度

- [x] CFS调度器
- [x] 实时调度器（FIFO、RR）
- [x] 单核调度
- [x] 多核调度
- [x] 负载均衡

### IPC

- [x] 匿名pipe管道
- [x] signal信号

### 文件系统

- [x] VFS
- [x] fat12/16/32
- [x] Devfs
- [x] RamFS
- [x] Procfs
- [x] Sysfs

### 异常及中断处理

- [x] APIC
- [x] softirq 软中断
- [x] 内核栈traceback


### 内核实用库

- [x] 字符串操作库
- [x] ELF可执行文件支持
- [x] printk
- [x] 基础数学库
- [x] 屏幕管理器
- [x] textui框架
- [x] CRC函数库
- [x] 通知链

### 系统调用

&emsp;&emsp;[请见系统调用文档](https://docs.dragonos.org/zh_CN/latest/syscall_api/index.html)

### 测试框架

- [x] ktest

### 驱动程序

- [x] ACPI 高级电源配置模块
- [x] IDE硬盘
- [x] AHCI硬盘
- [x] PCI、PCIe总线
- [x] XHCI（usb3.0）
- [x] ps/2 键盘
- [x] ps/2 鼠标
- [x] HPET高精度定时器
- [x] RTC时钟
- [x] local apic定时器
- [x] UART串口
- [x] VBE显示
- [x] VirtIO网卡
- [x] x87FPU
- [x] TTY终端
- [x] 浮点处理器


## 用户层

### LibC

- [x] 基础系统调用
- [x] 基础标准库函数
- [x] 部分数学函数

### shell命令行程序

- [x] 基于字符串匹配的解析
- [x] 基本的几个命令

### Http Server

- 使用C编写的简单的Http Server，能够运行静态网站。

## 软件移植

- [x] GCC 11.3.0 （暂时只支持了x86_64的Cross Compiler）[https://github.com/DragonOS-Community/gcc](https://github.com/DragonOS-Community/gcc)
- [x] binutils 2.38（暂时只支持了x86_64的Cross Compiler）[https://github.com/DragonOS-Community/binutils](https://github.com/DragonOS-Community/binutils)
- [x] gmp 6.2.1 [https://github.com/DragonOS-Community/gmp-6.2.1](https://github.com/DragonOS-Community/gmp-6.2.1)
- [x] mpfr 4.1.1 [https://github.com/DragonOS-Community/mpfr](https://github.com/DragonOS-Community/mpfr)
- [x] mpc 1.2.1 [https://github.com/DragonOS-Community/mpc](https://github.com/DragonOS-Community/mpc)
- [x] relibc [https://github.com/DragonOS-Community/relibc](https://github.com/DragonOS-Community/relibc)
- [x] sqlite3
