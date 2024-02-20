# 内核数据结构

&emsp;&emsp;内核中实现了常用的几种数据结构，这里是他们的api文档。

--------------
## kfifo先进先出缓冲区

&emsp;&emsp;kfifo先进先出缓冲区定义于`common/kfifo.h`中。您可以使用它，创建指定大小的fifo缓冲区（最大大小为4GB）

### kfifo_alloc

`int kfifo_alloc(struct kfifo_t *fifo, uint32_t size, uint64_t reserved)`

#### 描述

&emsp;&emsp;通过动态方式初始化kfifo缓冲队列。fifo缓冲区的buffer将由该函数进行申请。

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

**size**

&emsp;&emsp;缓冲区大小（单位：bytes）

**reserved**

&emsp;&emsp;当前字段保留，请将其置为0

#### 返回值

&emsp;&emsp;当返回值为0时，表示正常初始化成功，否则返回对应的errno

### kfifo_init

`void kfifo_init(struct kfifo_t *fifo, void *buffer, uint32_t size)`

#### 描述

&emsp;&emsp;使用指定的缓冲区来初始化kfifo缓冲队列

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

**buffer**

&emsp;&emsp;缓冲区基地址指针

**size**

&emsp;&emsp;缓冲区大小（单位：bytes）

### kfifo_free_alloc

`void kfifo_free_alloc(struct kfifo_t* fifo)`

#### 描述

&emsp;&emsp;释放通过kfifo_alloc创建的fifo缓冲区. 请勿通过该函数释放其他方式创建的kfifo缓冲区。

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

### kfifo_in

`uint32_t kfifo_in(struct kfifo_t *fifo, const void *from, uint32_t size)`

#### 描述

&emsp;&emsp;向kfifo缓冲区推入指定大小的数据。当队列中空间不足时，则不推入数据。

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

**from**

&emsp;&emsp;源数据基地址指针

**size**

&emsp;&emsp;数据大小（单位：bytes）

#### 返回值

&emsp;&emsp;返回成功被推入的数据的大小。

### kfifo_out

`uint32_t kfifo_out(struct kfifo_t *fifo, void *to, uint32_t size)`

#### 描述

&emsp;&emsp;从kfifo缓冲区取出数据，并从队列中删除数据。当队列中数据量不足时，则不取出。

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

**to**

&emsp;&emsp;目标缓冲区基地址指针

**size**

&emsp;&emsp;数据大小（单位：bytes）

#### 返回值

&emsp;&emsp;返回成功被取出的数据的大小。

### kfifo_out_peek

`uint32_t kfifo_out_peek(struct kfifo_t *fifo, void *to, uint32_t size)`

#### 描述

&emsp;&emsp;从kfifo缓冲区取出数据，但是不从队列中删除数据。当队列中数据量不足时，则不取出。

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

**to**

&emsp;&emsp;目标缓冲区基地址指针

**size**

&emsp;&emsp;数据大小（单位：bytes）

#### 返回值

&emsp;&emsp;返回成功被取出的数据的大小。

### kfifo_reset

`kfifo_reset(fifo)`

#### 描述

&emsp;&emsp;忽略kfifo队列中的所有内容，并把输入和输出偏移量都归零

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

### kfifo_reset_out

`kfifo_reset_out(fifo)`

#### 描述

&emsp;&emsp;忽略kfifo队列中的所有内容，并将输入偏移量赋值给输出偏移量

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

### kfifo_total_size

`kfifo_total_size(fifo)`

#### 描述

&emsp;&emsp;获取kfifo缓冲区的最大大小

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

#### 返回值

&emsp;&emsp;缓冲区最大大小

### kfifo_size

`kfifo_size(fifo)`

#### 描述

&emsp;&emsp;获取kfifo缓冲区当前已使用的大小

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

#### 返回值

&emsp;&emsp;缓冲区当前已使用的大小

### kfifo_empty

`kfifo_empty(fifo)`

#### 描述

&emsp;&emsp;判断kfifo缓冲区当前是否为空

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

#### 返回值

| 情况                      | 返回值 |
| ----------------------- | --- |
| 空 | 1   |
| 非空  | 0   |

### kfifo_full

`kfifo_full(fifo)`

#### 描述

&emsp;&emsp;判断kfifo缓冲区当前是否为满

#### 参数

**fifo**

&emsp;&emsp;kfifo队列结构体的指针

#### 返回值

| 情况  | 返回值 |
| ------| --- |
| 满 | 1   |
| 不满  | 0   |

------------------
