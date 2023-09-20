
# 如何使用GDB调试内核

## 前言
&emsp;&emsp;GDB是一个功能强大的开源调试工具，能够帮助您更好的诊断和修复程序中的错误。
&emsp;&emsp;它提供了一套丰富的功能，使您能够检查程序的执行状态、跟踪代码的执行流程、查看和修改变量的值、分析内存状态等。它可以与编译器配合使用，以便您在调试过程中访问程序的调试信息。

&emsp;&emsp;此教程将告诉您如何在DragonOS中使用`rust-gdb`来调试内核，包括如何开始调试以及相应的调试命令。

:::{note}
如果您已经熟悉了`rust-gdb`的各种命令，那您只需要阅读此教程的第一部分即可。
:::

---
## 1.从何开始

### 1.1 准备工作

&emsp;&emsp;在您开始调试内核之前，需要在/Kernel/Cargo.toml中开启调试模式，将Cargo.toml中的`debug = false`更改为`debug = true`。

```shell
debug = false
```
&emsp;&emsp;**更改为**
```shell
debug = true
```

### 1.2 运行DragonOS

&emsp;&emsp;准备工作完成后，您就可以编译、运行DragonOS来开展后续的调试工作了。
&emsp;&emsp;在DragonOS根目录中开启终端，使用`make run`即可开始编译运行DragonOS,如需更多编译命令方面的帮助，详见
> [构建DragonOS](https://docs.dragonos.org/zh_CN/latest/introduction/build_system.html)。

### 1.3 运行GDB
&emsp;&emsp;当DragonOS开始运行后，您就可以启动GDB开始调试了。

&emsp;&emsp;**您只需要开启一个新的终端，运行`make gdb`即可运行GDB调试器。**

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
若出现以上信息，输入c再回车即可。
:::

---

## 2.调试

### 2.1 开始

&emsp;&emsp;当以上步骤完成后，就已经可以开始调试了。

```shell
For help, type "help".
Type "apropos word" to search for commands related to "word".
warning: No executable has been specified and target does not support
determining executable automatically.  Try using the "file" command.
0xffff8000001f8f63 in ?? ()
(gdb)
```

:::{note}
GDB输出的信息中`0xffff8000001f8f63 in ?? ()`表明DragonOS还在引导加载的过程中。
:::

&emsp;&emsp;**输入`continue`或者`c`，程序将继续执行。**

```shell
For help, type "help".
Type "apropos word" to search for commands related to "word".
warning: No executable has been specified and target does not support
determining executable automatically.  Try using the "file" command.
0xffff8000001f8f63 in ?? ()
(gdb) continue
Continuing.
```

&emsp;&emsp;在DragonOS运行时，您可以随时按下`Ctrl+C`来发送中断信息。来查看内核当前状态。

```shell
(gdb) continue
Continuing.
^C
Thread 1 received signal SIGINT, Interrupt.
0xffff800000140c21 in io_in8 (port=113) at common/glib.h:136
136         __asm__ __volatile__("inb   %%dx,   %0      \n\t"
(gdb) 
```

### 2.2 设置断点和监视点

&emsp;&emsp;设置断点和监视点是程序调试中最基础的一步。

- **设置断点**

&emsp;&emsp;您可以使用`break`或者`b`命令来设置断点。

&emsp;&emsp;关于`break`或者`b`命令的使用:

```shell
b <line_number> #在当前活动源文件的相应行号打断点

b <file>:<line_number> #在对应文件的相应行号打断点

b <function_name> #为一个命名函数打断点
```

- **设置监视点**

&emsp;&emsp;您可以使用`watch`命令来设置监视点

```shell
watch <variable> # 设置对特定变量的监视点,将在特定变量发生变化的时候触发断点

watch <expression> # 设置对特定表达式的监视点，比如watch *(int*)0x12345678会在内存地址0x12345678处
                   # 的整数值发生更改时触发断点。
```

- **管理断点与监视点**

&emsp;&emsp;当我们打上断点之后，我们该如何查看我们所有的断点信息呢？

&emsp;&emsp;您可以通过`info b`，`info break`或者`info breakpoints`来查看所有的断点信息:

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

&emsp;&emsp;以上信息中，编号为12的断点即是我们在活动源文件309行打的断点，若其`Address`为`<MULTIPLE>`，则表示在多个地址上存在相同的断点位置。这在循环中是非常常见的情况。编号为13的便是我们对`slots`变量设置的监视点。

&emsp;&emsp;我们可以通过以下命令对断点或者监视点进行操作：

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

## 2.3 变量和内存查看

- **print 和 display** 

&emsp;&emsp;您可以通过`print`或者`p`来打印变量值。

&emsp;&emsp;`print`命令用于打印变量或表达式的值。它允许您在调试过程中查看程序中的数据。

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

&emsp;&emsp;您可以使用`display`命令来持续追踪变量或者表达式,`display`命令用于设置需要持续跟踪并在每次程序停止时显示的表达式。它类似于print命令，但与print不同的是，display命令在每次程序停止时自动打印指定表达式的值，而无需手动输入命令。

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

&emsp;&emsp;**要取消已设置的display命令并停止自动显示表达式的值，可以使用undisplay命令：**

```shell
undisplay <display编号> # 如果不指定<display编号>，则将取消所有已设置的display命令，
                       # 您可以通过info display命令来查看所有display编号
```

```{note}
请注意，print和display命令只会在程序暂停执行时评估变量或表达式的值。如果程序正在运行，您需要通过设置断点或使用其他调试命令来暂停程序，然后才能使用print命令查看数据的值,display命令设置的值将会在程序暂停时自动输出。
```

- **输出格式**

&emsp;&emsp;您可以设置输出格式来获取更多您需要的信息,例如：`print /a var`
> 参考至[GDB Cheat Sheet](https://darkdust.net/files/GDB%20Cheat%20Sheet.pdf)

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

### 2.4 查看调用堆栈

- **查看调用栈**

&emsp;&emsp;当程序在断点处暂停时，应该怎样追踪程序行为呢？

&emsp;&emsp;您可以通过`backtarce`命令来查看调用栈。`backtrace`命令用于打印当前调用栈的回溯信息。它显示了程序在执行过程中所有活动的函数调用链，包括每个函数的名称、参数和源文件中的行号。

```shell
# 示例输出
(gdb) backtrace 
#0  function1 (arg1=10, arg2=20) at file1.c:15
#1  function2 () at file2.c:25
#2  xx () at xx.c:8
```

&emsp;&emsp;每一行回溯信息都以#<frame_number>开头，指示帧的编号。然后是函数名和参数列表，最后是源文件名和行号。
通过查看回溯信息，您可以了解程序在哪些函数中执行，以及每个函数在调用栈中的位置。这对于调试程序和定位问题非常有用。

- **切换堆栈**

&emsp;&emsp;您可以通过`frame`或者`f`命令来切换对应的栈帧获取更多信息以及操作。

```shell
frame <frame_number>
f <frame_number>
```

&emsp;&emsp;除了简单地执行backtrace命令，还可以使用一些选项来自定义回溯信息的输出。例如：
```shell
backtrace full                          #显示完整的符号信息，包括函数参数和局部变量。
backtrace <frame_count>                 #限制回溯信息的帧数，只显示指定数量的帧。
backtrace <frame_start>-<frame_end>     #指定要显示的帧范围。
backtrace thread <thread_id>            #显示指定线程的回溯信息。
```

### 2.5 多核心

&emsp;&emsp;在调试内核时，您可能需要查看各个核心的运行状态。

&emsp;&emsp;您可以通过`info threads`命令来查看各个核心的运行状态

```shell
(gdb) info threads 
  Id   Target Id                    Frame 
  1    Thread 1.1 (CPU#0 [halted ]) 0xffff800000140a3e in Start_Kernel () at main.c:227
* 2    Thread 1.2 (CPU#1 [running]) thingbuf::Core::pop_ref<u8> ()
    at /home/heyicong/.cargo/registry/src/mirrors.tuna.tsinghua.edu.cn-df7c3c540f42cdbd/thingbuf-0.1.4/src/lib.rs:315
(gdb) 
```

&emsp;&emsp;您可以使用`thread <thread_id>`命令切换到指定的核心上下文，以便查看和调试特定核心的状态。例如：

```shell
(gdb) thread 1
[Switching to thread 1 (Thread 1.1)]
#0  0xffff800000140a3e in Start_Kernel () at main.c:227
227                 hlt();
```

### 2.6 更多

&emsp;&emsp;接下来，我将为您介绍更多您可能在调试中能够使用的命令：

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

## 最后

&emsp;&emsp;现在，您已经可以使用rust-gdb来调试DragonOS内核代码了。

> 您可以参阅GDB命令文档来获取更多帮助：[GDB Cheat Sheet](https://darkdust.net/files/GDB%20Cheat%20Sheet.pdf)