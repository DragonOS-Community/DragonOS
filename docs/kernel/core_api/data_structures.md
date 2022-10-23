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

## ID Allocation

&emsp;&emsp; ida的主要作用是分配+管理id. 它能分配一个最小的, 未被分配出去的id. 当您需要管理某个数据结构时, 可能需要使用id来区分不同的目标. 这个时候, ida将会是很好的选择. 因为ida的十分高效, 运行常数相对数组更小, 而且提供了基本管理id需要用到的功能, 值得您试一试.

&emsp;&emsp;IDA定义于`idr.h`文件中. 您通过`DECLARE_IDA(my_ida)`来创建一个ida对象, 或者`struct ida my_ida; ida_init(&my_ida);`来初始化一个ida. 

### ida_init
`void ida_init(struct ida *ida_p)`

#### 描述

&emsp;&emsp;通初始化IDA, 你需要保证调用函数之前, ida的free_list为空, 否则会导致内存泄漏. 
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

#### 返回值

&emsp;&emsp;无返回值

### ida_preload
`int ida_preload(struct ida *ida_p, gfp_t gfp_mask)`

#### 描述

&emsp;&emsp;为ida预分配空间.您可以不自行调用, 因为当ida需要空间的时候, 内部会自行使用`kmalloc`函数获取空间. 当然, 设计这个函数的目的是为了让您有更多的选择. 当您提前调用这个函数, 可以避免之后在开辟空间上的时间开销.
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

**gfp_mask**

&emsp;&emsp; 保留参数, 目前尚未使用.

#### 返回值

&emsp;&emsp;如果分配成功,将返回0; 否则返回负数错误码, 有可能是内存空间不够.


### ida_alloc
`int ida_alloc(struct ida *ida_p, int *p_id)`

#### 描述

&emsp;&emsp;获取一个空闲ID. 您需要注意, 返回值是成功/错误码.
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

**p_id**

&emsp;&emsp; 您需要传入一个int变量的指针, 如果成功分配ID, ID将会存储在该指针所指向的地址.

#### 返回值

&emsp;&emsp;如果分配成功,将返回0; 否则返回负数错误码, 有可能是内存空间不够.


### ida_count
`bool ida_count(struct ida *ida_p, int id)`

#### 描述

&emsp;&emsp;查询一个ID是否被分配.
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

**id**

&emsp;&emsp; 您查询该ID是否被分配.

#### 返回值

&emsp;&emsp;如果分配,将返回true; 否则返回false.



### ida_remove
`void ida_remove(struct ida *ida_p, int id)`

#### 描述

&emsp;&emsp;删除一个已经分配的ID. 如果该ID不存在, 该函数不会产生异常错误, 因为在检测到该ID不存在的时候, 函数将会自动退出.
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

**id**

&emsp;&emsp; 您要删除的id.

#### 返回值

&emsp;&emsp;无返回值. 

### ida_destroy
`void ida_destroy(struct ida *ida_p)`

#### 描述

&emsp;&emsp;释放一个IDA所有的空间, 同时删除ida的所有已经分配的id.(所以您不用担心删除id之后, ida还会占用大量空间.)
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

#### 返回值

&emsp;&emsp;无返回值

### ida_empty
`void ida_empty(struct ida *ida_p)`

#### 描述

&emsp;&emsp; 查询一个ida是否为空
#### 参数

**ida_p**

&emsp;&emsp; 指向ida的指针

#### 返回值

&emsp;&emsp;ida为空则返回true，否则返回false。


--------------------


## IDR

&emsp;&emsp; idr是一个基于radix-tree的ID-pointer的数据结构. 该数据结构提供了建id与数据指针绑定的功能, 它的主要功能有以下4个：
1. 获取一个ID, 并且将该ID与一个指针绑定  
2. 删除一个已分配的ID                
3. 根据ID查找对应的指针             
4. 根据ID使用新的ptr替换旧的ptr     
&emsp;&emsp; 您可以使用`DECLARE_idr(my_idr)`来创建一个idr。或者您也可以使用`struct idr my_idr; idr_init(my_idr);`这两句话创建一个idr。
&emsp;&emsp; 至于什么是radix-tree，您可以把他简单理解为一个向上生长的多叉树，在实现中，我们选取了64叉树。

### idr_init
`void idr_init(struct idr *idp)`

#### 描述

&emsp;&emsp;通初始化IDR, 你需要保证调用函数之前, idr的free_list为空, 否则会导致内存泄漏. 
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

#### 返回值

&emsp;&emsp;无返回值

### idr_preload
`int idr_preload(struct idr *idp, gfp_t gfp_mask)`

#### 描述

&emsp;&emsp;为idr预分配空间.您可以不自行调用, 因为当idr需要空间的时候, 内部会自行使用`kmalloc`函数获取空间. 当然, 设计这个函数的目的是为了让您有更多的选择. 当您提前调用这个函数, 可以避免之后在开辟空间上的时间开销.
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**gfp_mask**

&emsp;&emsp; 保留参数, 目前尚未使用.

#### 返回值

&emsp;&emsp;如果分配成功,将返回0; 否则返回负数错误码, 有可能是内存空间不够.


### idr_alloc
`int idr_alloc(struct idr *idp, void *ptr, int *id)`

#### 描述

&emsp;&emsp; 获取一个空闲ID. 您需要注意, 返回值是成功/错误码.
&emsp;&emsp; 调用这个函数，需要您保证ptr是非空的，即: `ptr != NULL`, 否则将会影响 `idr_find/idr_find_next/idr_find_next_getid/...`等函数的使用。(具体请看这三个函数的说明，当然，只会影响到您的使用体验，并不会影响到idr内部函数的决策和逻辑)
#### 参数

**idp**

&emsp;&emsp; 指向ida的指针

**ptr**

&emsp;&emsp; 指向数据的指针

**id**

&emsp;&emsp; 您需要传入一个int变量的指针, 如果成功分配ID, ID将会存储在该指针所指向的地址.

#### 返回值

&emsp;&emsp;如果分配成功,将返回0; 否则返回负数错误码, 有可能是内存空间不够.


### idr_remove
`void* idr_remove(struct idr *idp, int id)`

#### 描述

&emsp;&emsp;删除一个id, 但是不释放对应的ptr指向的空间, 同时返回这个被删除id所对应的ptr。
&emsp;&emsp; 如果该ID不存在, 该函数不会产生异常错误, 因为在检测到该ID不存在的时候, 函数将会自动退出，并返回NULL。
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**id**

&emsp;&emsp; 您要删除的id.

#### 返回值

&emsp;&emsp;如果删除成功，就返回被删除id所对应的ptr；否则返回NULL。注意：如果这个id本来就和NULL绑定，那么也会返回NULL


### idr_remove_all
`void idr_remove_all(struct idr *idp)`

#### 描述

&emsp;&emsp;删除idr的所有已经分配的id.(所以您不用担心删除id之后, idr还会占用大量空间。) 

&emsp;&emsp; 但是你需要注意的是，调用这个函数是不会释放数据指针指向的空间的。 所以您调用该函数之前， 确保IDR内部的数据指针被保存。否则当IDR删除所有ID之后， 将会造成内存泄漏。

#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

#### 返回值

&emsp;&emsp;无返回值


### idr_destroy
`void idr_destroy(struct idr *idp)`

#### 描述

&emsp;&emsp;释放一个IDR所有的空间, 同时删除idr的所有已经分配的id.(所以您不用担心删除id之后, ida还会占用大量空间.) - 和`idr_remove_all`的区别是， 释放掉所有的空间(包括free_list的预分配空间)。
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

#### 返回值

&emsp;&emsp;无返回值


### idr_find
`void *idr_find(struct idr *idp, int id)`

#### 描述

&emsp;&emsp;查询一个ID所绑定的数据指针
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**id**

&emsp;&emsp; 您查询该ID的数据指针

#### 返回值

&emsp;&emsp; 如果分配,将返回该ID对应的数据指针; 否则返回NULL.(注意， 返回NULL不一定代表这ID不存在，有可能该ID就是与空指针绑定。)
&emsp;&emsp; 当然，我们也提供了`idr_count`函数来判断id是否被分配，具体请查看idr_count介绍。

### idr_find_next
`void *idr_find_next(struct idr *idp, int start_id)`

#### 描述

&emsp;&emsp;传进一个start_id，返回满足 "id大于start_id的最小id" 所对应的数据指针。
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**start_id**

&emsp;&emsp;您提供的ID限制

#### 返回值

&emsp;&emsp; 如果分配,将返回该ID对应的数据指针; 否则返回NULL.(注意， 返回NULL不一定代表这ID不存在，有可能该ID就是与空指针绑定。)
&emsp;&emsp; 当然，我们也提供了`idr_count`函数来判断id是否被分配，具体请查看idr_count介绍。


### idr_find_next_getid
`void *idr_find_next_getid(struct idr *idp, int start_id, int *nextid)`

#### 描述

&emsp;&emsp;传进一个start_id，返回满足 "id大于start_id的最小id" 所对应的数据指针。同时，你获取到这个满足条件的最小id， 即参数中的 *nextid。
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**start_id**

&emsp;&emsp; 您提供的ID限制

#### 返回值

&emsp;&emsp; 如果分配,将返回该ID对应的数据指针; 否则返回NULL.(注意， 返回NULL不一定代表这ID不存在，有可能该ID就是与空指针绑定。)
&emsp;&emsp; 当然，我们也提供了`idr_count`函数来判断id是否被分配，具体请查看idr_count介绍。


### idr_replace
`int idr_replace(struct idr *idp, void *ptr, int id)`

#### 描述

&emsp;&emsp;传进一个ptr，使用该ptr替换掉id所对应的Old_ptr。
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**ptr**

&emsp;&emsp;您要替换原来的old_ptr的新指针

**id**

&emsp;&emsp; 您要替换的指针所对应的id

#### 返回值

&emsp;&emsp; 0代表成功，否则就是错误码 - 代表错误。 


### idr_replace_get_old
`int idr_replace_get_old(struct idr *idp, void *ptr, int id, void **oldptr)`

#### 描述

&emsp;&emsp;传进一个ptr，使用该ptr替换掉id所对应的Old_ptr，同时你可以获取到old_ptr。
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

**ptr**

&emsp;&emsp;您要替换原来的old_ptr的新指针

**id**

&emsp;&emsp; 您要替换的指针所对应的id


**old_ptr**

&emsp;&emsp; 您需要传进该(void**)指针，old_ptr将会存放在该指针所指向的地址。


#### 返回值

&emsp;&emsp; 0代表成功，否则就是错误码 - 代表错误。 

### idr_empty
`void idr_empty(struct idr *idp)`

#### 描述

&emsp;&emsp; 查询一个idr是否为空
#### 参数

**idp**

&emsp;&emsp; 指向idr的指针

#### 返回值

&emsp;&emsp;idr为空则返回true，否则返回false。

### idr_count
`bool idr_count(struct idr *idp, int id)`

#### 描述

&emsp;&emsp;查询一个ID是否被分配.
#### 参数

**ida_p**

&emsp;&emsp; 指向idr的指针

**id**

&emsp;&emsp; 您查询该ID是否被分配.

#### 返回值

&emsp;&emsp;如果分配,将返回true; 否则返回false.