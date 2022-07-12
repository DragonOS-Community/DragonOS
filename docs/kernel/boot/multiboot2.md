# Multiboot2支持模块

&emsp;&emsp;Multiboot2支持模块提供对Multiboot2协议的支持。位于`kernel/driver/multiboot2`文件夹中。

&emsp;&emsp;根据Multiboot2协议，操作系统能够从BootLoader处获得一些信息，比如基本的硬件信息以及ACPI表的起始地址等。

---

## 数据结构

&emsp;&emsp;`kernel/driver/multiboot2/multiboot2.h`中按照Multiboot2协议的规定，定义了大部分的数据结构，具体细节可查看该文件: [DragonOS/multiboot2.h at master · fslongjin/DragonOS · GitHub](https://github.com/fslongjin/DragonOS/blob/master/kernel/driver/multiboot2/multiboot2.h)

---

## 迭代器

&emsp;&emsp;由于Multiboot2的信息存储在自`multiboot2_boot_info_addr`开始的一段连续的内存空间之中，且不同类型的header的长度不同，因此设计了一迭代器`multiboot2_iter`。

### 函数原型

```c
void multiboot2_iter(bool (*_fun)(const struct iter_data_t *, void *, unsigned int *),
                     void *data, unsigned int *count)
```

**_fun**

&emsp;&emsp;指定的handler。当某个header的tag与该handler所处理的tag相同时，handler将处理该header，并返回true。

&emsp;&emsp;其第一个参数为tag类型，第二个参数为返回的数据的指针，第三个值为计数（某些没有用到该值的地方，该值可以为空）

**data**

&emsp;&emsp;传递给`_fun`的第二个参数，`_fun`函数将填充该指针所指向的内存区域，从而返回数据。

**count**

&emsp;&emsp;当返回的**data**为一个列表时，通过该值来指示列表中有多少项。

---

## 迭代工作函数

&emsp;&emsp;在模块中，按照我们需要获取不同类型的tag的需要，定义了一些迭代器工作函数。


