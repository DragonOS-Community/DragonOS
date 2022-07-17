# 内核栈traceback

## 简介

&emsp;&emsp;内核栈traceback的功能位于`kernel/debug/traceback/`文件夹中。为内核态提供traceback的功能，打印调用栈到屏幕上。

---

## API

### `void traceback(struct pt_regs * regs)`

#### 作用

&emsp;&emsp;该接口定义于`kernel/debug/traceback/traceback.h`中，将会对给定内核栈进行traceback，并打印跟踪结果到屏幕上。

#### 参数

##### regs

&emsp;&emsp;要开始追踪的第一层内核栈栈帧（也就是栈的底端）

---

## 实现原理

&emsp;&emsp;当内核第一次链接之后，将会通过Makefile中的命令，运行`kernel/debug/kallsyms`程序，提取内核文件的符号表，然后生成`kernel/debug/kallsyms.S`。该文件的rodata段中存储了text段的函数的符号表。接着，该文件将被编译为`kallsyms.o`。最后，Makefile中再次调用`ld`命令进行链接，将kallsyms.o链接至内核文件。

&emsp;&emsp;当调用`traceback`函数时，其将遍历该符号表，找到对应的符号并输出。

---

## 未来发展方向

- 增加写入到日志文件的功能