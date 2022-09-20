# DragonOS内核核心API

## 循环链表管理函数

&emsp;&emsp;循环链表是内核的重要的数据结构之一。包含在`kernel/common/glib.h`中。

### `void list_init(struct List *list)`

#### 描述

&emsp;&emsp;初始化一个List结构体，使其prev和next指针指向自身

#### 参数

**list**

&emsp;&emsp;要被初始化的List结构体

### `void list_add(struct List *entry, struct List *node)`

#### 描述

&emsp;&emsp;将node插入到entry的后方

#### 参数

**entry**

&emsp;&emsp;已存在于循环链表中的一个结点

**node**

&emsp;&emsp;待插入的结点

### `void list_append(struct List *entry, struct List *node)`

#### 描述

&emsp;&emsp;将node插入到entry的前方

#### 参数

**entry**

&emsp;&emsp;已存在于循环链表中的一个结点

**node**

&emsp;&emsp;待插入的结点

### `void list_del(struct List *entry)`

#### 描述

&emsp;&emsp;从链表中删除结点entry

#### 参数

**entry**

&emsp;&emsp;待删除的结点

### `bool list_empty(struct List *entry)`

#### 描述

&emsp;&emsp;判断链表是否为空

#### 参数

**entry**

&emsp;&emsp;链表中的一个结点

### `struct List *list_prev(struct List *entry)`

#### 描述

&emsp;&emsp;获取entry的前一个结点

#### 参数

**entry**

&emsp;&emsp;链表中的一个结点

### `struct List *list_next(struct List *entry)`

#### 描述

&emsp;&emsp;获取entry的后一个结点

#### 参数

**entry**

&emsp;&emsp;链表中的一个结点

---

## 基础C函数库

&emsp;&emsp;内核编程与应用层编程不同，你将无法使用LibC中的函数来进行编程。为此，内核实现了一些常用的C语言函数，并尽量使其与标准C库中的函数行为相近。值得注意的是，这些函数的行为可能与标准C库函数不同，请在使用时仔细阅读以下文档，这将会为你带来帮助。

### 字符串操作

#### `int strlen(const char *s)`

##### 描述

&emsp;&emsp;测量并返回字符串长度。

##### 参数

**src**

&emsp;&emsp;源字符串

#### `long strnlen(const char *src, unsigned long maxlen)`

##### 描述

&emsp;&emsp;测量并返回字符串长度。当字符串长度大于maxlen时，返回maxlen

##### 参数

**src**

&emsp;&emsp;源字符串

**maxlen**

&emsp;&emsp;最大长度

#### `long strnlen_user(const char *src, unsigned long maxlen)`

##### 描述

&emsp;&emsp;测量并返回字符串长度。当字符串长度大于maxlen时，返回maxlen。

&emsp;&emsp;该函数会进行地址空间校验，要求src字符串必须来自用户空间。当源字符串来自内核空间时，将返回0.

##### 参数

**src**

&emsp;&emsp;源字符串，地址位于用户空间

**maxlen**

&emsp;&emsp;最大长度

#### `char *strncpy(char *dst, const char *src, long count)`

##### 描述

&emsp;&emsp;拷贝长度为count个字节的字符串，返回dst字符串

##### 参数

**src**

&emsp;&emsp;源字符串

**dst**

&emsp;&emsp;目标字符串

**count**

&emsp;&emsp;要拷贝的源字符串的长度

#### `char *strcpy(char *dst, const char *src)`

##### 描述

&emsp;&emsp;拷贝源字符串，返回dst字符串

##### 参数

**src**

&emsp;&emsp;源字符串

**dst**

&emsp;&emsp;目标字符串

#### `long strncpy_from_user(char *dst, const char *src, unsigned long size)`

##### 描述

&emsp;&emsp;从用户空间拷贝长度为count个字节的字符串到内核空间，返回拷贝的字符串的大小

&emsp;&emsp;该函数会对字符串的地址空间进行校验，防止出现地址空间越界的问题。

##### 参数

**src**

&emsp;&emsp;源字符串

**dst**

&emsp;&emsp;目标字符串

**size**

&emsp;&emsp;要拷贝的源字符串的长度

#### `int strcmp(char *FirstPart, char *SecondPart)`

##### 描述

  比较两个字符串的大小。

***返回值***

| 情况                      | 返回值 |
| ----------------------- | --- |
| FirstPart == SecondPart | 0   |
| FirstPart > SecondPart  | 1   |
| FirstPart < SecondPart  | -1  |

##### 参数

**FirstPart**

&emsp;&emsp;第一个字符串

**SecondPart**

&emsp;&emsp;第二个字符串

#### `printk(const char* fmt, ...)`

##### 描述

&emsp;&emsp;该宏能够在控制台上以黑底白字格式化输出字符串.

##### 参数

**fmt**

&emsp;&emsp;源格式字符串

**...**

&emsp;&emsp;可变参数

#### `printk_color(unsigned int FRcolor, unsigned int BKcolor, const char* fmt, ...)`

##### 描述

&emsp;&emsp;在控制台上以指定前景色和背景色格式化输出字符串.

##### 参数

**FRcolor**

&emsp;&emsp;前景色

**BKcolor**

&emsp;&emsp;背景色

**fmt**

&emsp;&emsp;源格式字符串

**...**

&emsp;&emsp;可变参数

#### `int vsprintf(char *buf, const char *fmt, va_list args)`

##### 描述

&emsp;&emsp;按照fmt格式化字符串，并将结果输出到buf中，返回写入buf的字符数量。

##### 参数

**buf**

&emsp;&emsp;输出缓冲区

**fmt**

&emsp;&emsp;源格式字符串

**args**

&emsp;&emsp;可变参数列表

#### `int sprintk(char *buf, const char *fmt, ...)`

##### 描述

&emsp;&emsp;按照fmt格式化字符串，并将结果输出到buf中，返回写入buf的字符数量。

##### 参数

**buf**

&emsp;&emsp;输出缓冲区

**fmt**

&emsp;&emsp;源格式字符串

**...**

&emsp;&emsp;可变参数

### 内存操作

#### `void *memcpy(void *dst, const void *src, uint64_t size)`

##### 描述

&emsp;&emsp;将内存从src处拷贝到dst处。

##### 参数

**dst**

&emsp;&emsp;指向目标地址的指针

**src**

&emsp;&emsp;指向源地址的指针

**size**

&emsp;&emsp;待拷贝的数据大小

#### `void *memmove(void *dst, const void *src, uint64_t size)`

##### 描述

&emsp;&emsp;与`memcpy()`类似，但是在源数据区域与目标数据区域之间存在重合时，该函数能防止数据被错误的覆盖。

##### 参数

**dst**

&emsp;&emsp;指向目标地址的指针

**src**

&emsp;&emsp;指向源地址的指针

**size**

&emsp;&emsp;待拷贝的数据大小

## CRC函数

### 函数列表

**`uint8_t crc7(uint8_t crc, const uint8_t *buffer, size_t len)`**

**`uint8_t crc8(uint8_t crc, const uint8_t *buffer, size_t len)`**

**`uint16_t crc16(uint16_t crc, uint8_t const *buffer, size_t len)`**

**`uint32_t crc32(uint32_t crc, uint8_t const *buffer, size_t len)`**

**`uint64_t crc64(uint64_t crc, uint8_t const *buffer, size_t len)`**

### 描述

&emsp;&emsp;用于计算循环冗余校验码

### 参数说明

**crc**

&emsp;&emsp;传入的CRC初始值

**buffer**

&emsp;&emsp;待处理的数据缓冲区

**len**

&emsp;&emsp;缓冲区大小（字节）

