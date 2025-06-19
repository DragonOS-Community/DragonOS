:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/debug/debug-kernel-with-gdb.md

- Translation time: 2025-05-19 01:41:41

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# How to Use GDB to Debug the Kernel

## Introduction
&emsp;&emsp;GDB is a powerful open-source debugging tool that can help you better diagnose and fix errors in programs.

&emsp;&emsp;It provides a rich set of features that allow you to check the execution status of a program, track the execution flow of code, view and modify the values of variables, analyze memory states, and more. It can be used in conjunction with a compiler to allow you to access debugging information during the debugging process.

&emsp;&emsp;This tutorial will guide you on how to use `rust-gdb` to debug the kernel in DragonOS, including how to start debugging and the corresponding debugging commands.

:::{note}
If you are already familiar with the various commands of `rust-gdb`, you only need to read the first part of this tutorial.
:::

---
## 1. Getting Started

### 1.1 Preparation

&emsp;&emsp;Before you start debugging the kernel, you need to enable debug mode in /Kernel/Cargo.toml by changing `debug = false` to `debug = true` in the Cargo.toml file.

```shell
debug = false
```
&emsp;&emsp;**Change to**
```shell
debug = true
```

### 1.2 Running DragonOS

&emsp;&emsp;After the preparation is complete, you can compile and run DragonOS to proceed with the subsequent debugging work.

&emsp;&emsp;Open a terminal in the root directory of DragonOS and use `make run` to start compiling and running DragonOS. For more help with compilation commands, see
> [Building DragonOS](https://docs.dragonos.org/zh_CN/latest/introduction/build_system.html).

### 1.3 Running GDB
&emsp;&emsp;Once DragonOS has started running, you can start debugging with GDB.

&emsp;&emsp;**You only need to open a new terminal and run `make gdb` to start the GDB debugger.**

```shell
❯ make gdb
rust-gdb -n -x tools/.gdbinit
GNU gdb (Ubuntu 12.1-0ubuntu1~22.04) 12.1
Copyright (C) 2022 Free Software Foundation, Inc.
License GPLv3+: GNU GPL version 3 or later <http://gnu.org/licenses/gpl.html>
This is free software: you are free to change and redistribute it.
There is NO WARRANTY, to the extent permitted by law.
Type "show copying" and "show warranty" for details.
This GDB was configured as "x86_64-linux-gnu".
Type "show configuration" for configuration details.
For bug reporting instructions, please see:
<https://www.gnu.org/software/gdb/bugs/>.
Find the GDB manual and other documentation resources online at:
    <http://www.gnu.org/software/gdb/documentation/>.

--Type <RET> for more, q to quit, c to continue without paging--
```

:::{note}
If you see the above information, input `c` and press Enter.
:::

---

## 2. Debugging

### 2.1 Start

&emsp;&emsp;After completing the above steps, you can start debugging.

```shell
For help, type "help".
Type "apropos word" to search for commands related to "word".
warning: No executable has been specified and target does not support
determining executable automatically.  Try using the "file" command.
0xffff8000001f8f63 in ?? ()
(gdb)
```

:::{note}
The output information from GDB, `0xffff8000001f8f63 in ?? ()`, indicates that DragonOS is still in the process of booting.
:::

&emsp;&emsp;**Input `continue` or `c` to continue the program execution.**

```shell
For help, type "help".
Type "apropos word" to search for commands related to "word".
warning: No executable has been specified and target does not support
determining executable automatically.  Try using the "file" command.
0xffff8000001f8f63 in ?? ()
(gdb) continue
Continuing.
```

&emsp;&emsp;While DragonOS is running, you can press `Ctrl+C` at any time to send an interrupt signal to view the current state of the kernel.

```shell
(gdb) continue
Continuing.
^C
Thread 1 received signal SIGINT, Interrupt.
0xffff800000140c21 in io_in8 (port=113) at common/glib.h:136
136         __asm__ __volatile__("inb   %%dx,   %0      \n\t"
(gdb) 
```

### 2.2 Setting Breakpoints and Watchpoints

&emsp;&emsp;Setting breakpoints and watchpoints is the most fundamental step in program debugging.

- **Setting Breakpoints**

&emsp;&emsp;You can use the `break` or `b` command to set a breakpoint.

&emsp;&emsp;Regarding the usage of `break` or `b` commands:

```shell
b <line_number> #在当前活动源文件的相应行号打断点

b <file>:<line_number> #在对应文件的相应行号打断点

b <function_name> #为一个命名函数打断点
```

- **Setting Watchpoints**

&emsp;&emsp;You can use the `watch` command to set a watchpoint.

```shell
watch <variable> # 设置对特定变量的监视点,将在特定变量发生变化的时候触发断点

watch <expression> # 设置对特定表达式的监视点，比如watch *(int*)0x12345678会在内存地址0x12345678处
                   # 的整数值发生更改时触发断点。
```

- **Managing Breakpoints and Watchpoints**

&emsp;&emsp;Once we have set breakpoints, how can we view all the breakpoint information?

&emsp;&emsp;You can use `info b`, `info break`, or `info breakpoints` to view all breakpoint information:

```shell
(gdb) b 309
Breakpoint 12 at 0xffff8000001f8f16: file /home/heyicong/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-df7c3c540f42cdbd/thingbuf-0.1.4/src/lib.rs, line 315.
(gdb) watch slots
Watchpoint 13: slots
(gdb) info b
Num     Type           Disp Enb Address            What
12      breakpoint     keep y   0xffff8000001f8f16 in thingbuf::Core::pop_ref<u8> 
                                                   at /home/heyicong/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-df7c3c540f42cdbd/thingbuf-0.1.4/src/lib.rs:315
13      watchpoint     keep y                      slots
(gdb) 
```

&emsp;&emsp;In the above information, the breakpoint with number 12 is the one we set in line 309 of the active source file. If its `Address` is `<MULTIPLE>`, it indicates that there are identical breakpoints at multiple addresses. This is very common in loops. The breakpoint with number 13 is the watchpoint we set for the `slots` variable.

&emsp;&emsp;We can perform operations on breakpoints or watchpoints using the following commands:

```shell
delete <breakpoint#> # 或 d <breakpoint#> 删除对应编号的断点，在您不再需要使用这个断点的时候可以通过此命令删除断点
delete <watchpoint#> # 或 d <watchpoint##> 删除对应编号的监视点，在您不再需要使用这个监视点的时候可以通过此命令删除监视点

disable <breakpoint#> # 禁用对应编号的断点，这适合于您只是暂时不需要使用这个断点时使用，当您禁用一个断点，下
                      # 次程序运行到该断点处将不会停下来
disable <watchpoint#> # 禁用对应编号的监视点，这适合于您只是暂时不需要使用这个监视点时使用

enable <breakpoint#> # 启用对应编号的断点
enable <watchpoint#> # 启用对应编号的监视点

#clear命令
clear # 清除当前活动源文件的断点以及监视点
clear <point_number> # 清除对应编号的所有断点或监视点，这与delete行为是一致的
clear <file> # 清除指定文件的所有断点与监视点
```

## 2.3 Viewing Variables and Memory

- **print and display**

&emsp;&emsp;You can use `print` or `p` to print variable values.

&emsp;&emsp;The `print` command is used to print the value of a variable or expression. It allows you to view the data in the program during debugging.

```shell
print <variable> # 打印对应变量名的值，例如：print my_variable 或者 p my_variable

print <expression> # 打印合法表达式的值，例如：print a+b 或者 p a+b

# 示例输出
(gdb) print order
$3 = core::sync::atomic::Ordering::SeqCst
```

```{note}
如果您不仅想打印值，还想显示更多详细信息（例如类型信息），可以使用ptype命令。
```

&emsp;&emsp;You can use the `display` command to continuously track variables or expressions. The `display` command is used to set expressions that need to be tracked and displayed every time the program stops. It is similar to the print command, but unlike print, the display command automatically prints the value of the specified expression every time the program stops, without requiring manual input of a command.

```shell
display <variable> # 打印对应变量名的值，例如：display my_variable

display <expression> # 打印合法表达式的值，例如：display a+b

# 示例输出
(gdb) display order
1: order = core::sync::atomic::Ordering::SeqCst #其中1表示display编号，
                                                #您可以通过info display命令来查看所有display编号
```

```{note}
一旦您设置了display命令，每当程序停止（例如，在断点处停止）时，GDB将自动打印指定表达式的值。

display命令非常有用，因为它允许您在调试过程中持续监视表达式的值，而无需每次都手动输入print命令。它特别适用于那些您希望持续跟踪的变量或表达式。
```

&emsp;&emsp;**To cancel an already set display command and stop automatically displaying the value of an expression, you can use the undisplay command:**

```shell
undisplay <display编号> # 如果不指定<display编号>，则将取消所有已设置的display命令，
                       # 您可以通过info display命令来查看所有display编号
```

```{note}
请注意，print和display命令只会在程序暂停执行时评估变量或表达式的值。如果程序正在运行，您需要通过设置断点或使用其他调试命令来暂停程序，然后才能使用print命令查看数据的值,display命令设置的值将会在程序暂停时自动输出。
```

- **Output Format**

&emsp;&emsp;You can set the output format to get more information you need, for example: `print /a var`
> Refer to [GDB Cheat Sheet](https://darkdust.net/files/GDB%20Cheat%20Sheet.pdf)

```shell
Format
a Pointer.
c Read as integer, print as character.
d Integer, signed decimal.
f Floating point number.
o Integer, print as octal.
s Try to treat as C string.
t Integer, print as binary (t = „two“).
u Integer, unsigned decimal.
x Integer, print as hexadecimal.
```

### 2.4 Viewing the Call Stack

- **Viewing the Call Stack**

&emsp;&emsp;When the program is paused at a breakpoint, how should you trace the program's behavior?

&emsp;&emsp;You can use the `backtarce` command to view the call stack. The `backtrace` command is used to print the backtrace information of the current call stack. It displays all the active function call chains during program execution, including the function names, parameters, and line numbers in the source files.

```shell
# 示例输出
(gdb) backtrace 
#0  function1 (arg1=10, arg2=20) at file1.c:15
#1  function2 () at file2.c:25
#2  xx () at xx.c:8
```

&emsp;&emsp;Each line of backtrace information starts with #<frame_number>, indicating the frame number. Then comes the function name and parameter list, followed by the source file name and line number.
By viewing the backtrace information, you can understand in which functions the program is executing and the position of each function in the call stack. This is very useful for debugging the program and locating problems.

- **Switching the Stack**

&emsp;&emsp;You can use the `frame` or `f` command to switch to the corresponding stack frame to get more information and perform operations.

```shell
frame <frame_number>
f <frame_number>
```

&emsp;&emsp;In addition to simply executing the backtrace command, you can also use some options to customize the output of the backtrace information. For example:
```shell
backtrace full                          #显示完整的符号信息，包括函数参数和局部变量。
backtrace <frame_count>                 #限制回溯信息的帧数，只显示指定数量的帧。
backtrace <frame_start>-<frame_end>     #指定要显示的帧范围。
backtrace thread <thread_id>            #显示指定线程的回溯信息。
```

### 2.5 Multi-core

&emsp;&emsp;When debugging the kernel, you may need to view the running status of each core.

&emsp;&emsp;You can use the `info threads` command to view the running status of each core.

```shell
(gdb) info threads 
  Id   Target Id                    Frame 
  1    Thread 1.1 (CPU#0 [halted ]) 0xffff800000140a3e in Start_Kernel () at main.c:227
* 2    Thread 1.2 (CPU#1 [running]) thingbuf::Core::pop_ref<u8> ()
    at /home/heyicong/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-df7c3c540f42cdbd/thingbuf-0.1.4/src/lib.rs:315
(gdb) 
```

&emsp;&emsp;You can use the `thread <thread_id>` command to switch to the context of a specific core to view and debug the status of that core. For example:

```shell
(gdb) thread 1
[Switching to thread 1 (Thread 1.1)]
#0  0xffff800000140a3e in Start_Kernel () at main.c:227
227                 hlt();
```

### 2.6 More

&emsp;&emsp;Next, I will introduce more commands that you may find useful during debugging:

```shell
step                #或者s,逐行执行程序，并进入到函数调用中。可以在step命令后加执行次数，例：step 3 表示要连续执行3个步骤
step <function>     #进入指定的函数，并停止在函数内的第一行。

next                #或者n,逐行执行程序，但跳过函数调用，直接执行函数调用后的下一行代码。
                    #它允许你在不进入函数内部的情况下执行代码，从而快速跳过函数调用的细节。
                    #同样，next也可以在命令后加执行次数

finish              #用于从当前函数中一直执行到函数返回为止，并停在调用该函数的地方。
                    #它允许你快速执行完当前函数的剩余部分，并返回到调用函数的上下文中。  

continue            #用于继续程序的执行，直到遇到下一个断点或
                    #程序正常结束或者程序暂停。 

quit                #退出调试                    

list                            #或者l，显示当前活动源文件源代码的片段，以及当前执行的位置。
list <filename>:<function>      #显示<filename>文件里面的<funtion>函数的源代码片段
list <filename>:<line_number>   #显示<filename>文件里面的<line_number>附近的源代码片段
list <first>,<last>             #显示当前活动源文件的<first>至<last>之间的源代码片段
set listsize <count>            #设置list命令显示的源代码行数。默认情况下，list命令显示当前行和其周围的几行代码。

info args                       #显示当前函数的参数及其值
info breakpoints                #显示断点以及监视点信息
info display                    #显示当前设置的display列表
info locals                     #显示当前函数/栈帧中的局部变量及其值
info sharedlibrary              #显示当前已加载的共享库（shared library）信息
info signals                    #显示当前程序所支持的信号信息。它可以列出程序可以接收和处理的不同信号的列表。
info threads                    #显示各个核心/线程信息，它可以列出当前正在运行的核心/线程以及它们的状态。

show directories                #显示当前源代码文件的搜索路径列表。这些搜索路径决定了GDB在查找源代码文件时的搜索范围。
show listsize                   #显示打印源代码时的上下文行数。它确定了在使用list命令（或其简写形式l）时显示的源代码行数。

whatis variable_name            #查看给定变量或表达式的类型信息。它可以帮助你了解变量的数据类型。
ptype                           #显示给定类型或变量的详细类型信息。它可以帮助你了解类型的结构和成员。
                                #相较于whatis命令，ptype命令更加详细。

set var <variable_name>=<value> #设置变量值

return <expression>             #强制使当前函数返回设定值                                                  
```

---

## Conclusion

&emsp;&emsp;Now, you can use rust-gdb to debug the DragonOS kernel code.

> You can refer to the GDB command documentation for more help: [GDB Cheat Sheet](https://darkdust.net/files/GDB%20Cheat%20Sheet.pdf)
