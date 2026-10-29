# DragonOS内核核心API

## 循环链表管理函数

循环链表是内核的重要的数据结构之一。包含在`kernel/common/list.h`中。

### `void list_init(struct List *list)`

#### 描述

初始化一个List结构体，使其prev和next指针指向自身

#### 参数

**list**

要被初始化的List结构体

### `void list_add(struct List *entry, struct List *node)`

#### 描述

将node插入到entry的后方

#### 参数

**entry**

已存在于循环链表中的一个结点

**node**

待插入的结点

### `void list_append(struct List *entry, struct List *node)`

#### 描述

将node插入到entry的前方

#### 参数

**entry**

已存在于循环链表中的一个结点

**node**

待插入的结点

### `void list_del(struct List *entry)`

#### 描述

从链表中删除结点entry

#### 参数

**entry**

待删除的结点

### `list_del_init(struct List *entry)`

#### 描述

从链表中删除结点entry，并将这个entry使用list_init()进行重新初始化。

#### 参数

**entry**

待删除的结点

### `bool list_empty(struct List *entry)`

#### 描述

判断链表是否为空

#### 参数

**entry**

链表中的一个结点

### `struct List *list_prev(struct List *entry)`

#### 描述

获取entry的前一个结点

#### 参数

**entry**

链表中的一个结点

### `struct List *list_next(struct List *entry)`

#### 描述

获取entry的后一个结点

#### 参数

**entry**

链表中的一个结点

### `void list_replace(struct List *old, struct List *new)`

#### 描述

将链表中的old结点替换成new结点

#### 参数

**old**

要被换下来的结点

**new**

要被换入链表的新的结点


### `list_entry(ptr, type, member)` {#_list_entry}

#### 描述

该宏能通过ptr指向的List获取到List所处的结构体的地址

#### 参数

**ptr**

指向List结构体的指针

**type**

要被换入链表的新的结点

**member**

List结构体在上述的“包裹list结构体的结构体”中的变量名

### `list_first_entry(ptr, type, member)`

#### 描述

获取链表中的第一个元素。请注意，该宏要求链表非空，否则会出错。

#### 参数

与[list_entry() ](/zh/kernel/core_api/kernel_api.html#_list_entry)相同

### `list_first_entry_or_null(ptr, type, member)`

#### 描述

获取链表中的第一个元素。若链表为空，则返回NULL。

#### 参数

与[list_entry() ](/zh/kernel/core_api/kernel_api.html#_list_entry)相同

### `list_last_entry(ptr, type, member)`

#### 描述

获取链表中的最后一个元素。请注意，该宏要求链表非空，否则会出错。

#### 参数

与[list_entry() ](/zh/kernel/core_api/kernel_api.html#_list_entry)相同

### `list_last_entry_or_full(ptr, type, member)`

#### 描述

获取链表中的最后一个元素。若链表为空，则返回NULL。

#### 参数

与[list_entry() ](/zh/kernel/core_api/kernel_api.html#_list_entry)相同

### `list_next_entry(pos, member)` {#_list_next_entry}

#### 描述

获取链表中的下一个元素

#### 参数

**pos**

指向当前的外层结构体的指针

**member**

链表结构体在外层结构体内的变量名

### `list_prev_entry(pos, member)`

#### 描述

获取链表中的上一个元素

#### 参数

与[list_next_entry() ](/zh/kernel/core_api/kernel_api.html#_list_next_entry)相同

### `list_for_each(ptr, head)` {#_list_for_each}

#### 描述

遍历整个链表（从前往后）

#### 参数

**ptr**

指向List结构体的指针

**head**

指向链表头结点的指针(struct List*)

### `list_for_each_prev(ptr, head)`

#### 描述

遍历整个链表（从后往前）

#### 参数

与[list_for_each() ](/zh/kernel/core_api/kernel_api.html#_list_for_each)相同

### `list_for_each_safe(ptr, n, head)` {#_list_for_each_safe}

#### 描述

从前往后遍历整个链表（支持删除当前链表结点）

该宏通过暂存中间变量，防止在迭代链表的过程中，由于删除了当前ptr所指向的链表结点从而造成错误.

#### 参数

**ptr**

指向List结构体的指针

**n**

用于存储临时值的List类型的指针

**head**

指向链表头结点的指针(struct List*)

### `list_for_each_prev_safe(ptr, n, head)`

#### 描述

从后往前遍历整个链表.（支持删除当前链表结点）

该宏通过暂存中间变量，防止在迭代链表的过程中，由于删除了当前ptr所指向的链表结点从而造成错误.

#### 参数

与[list_for_each_safe() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_safe)相同

### `list_for_each_entry(pos, head, member)` {#_list_for_each_entry}

#### 描述

从头开始迭代给定类型的链表

#### 参数

**pos**

指向特定类型的结构体的指针

**head**

指向链表头结点的指针(struct List*)

**member**

struct List在pos的结构体中的成员变量名

### `list_for_each_entry_reverse(pos, head, member)`

#### 描述

逆序迭代给定类型的链表

#### 参数

与[list_for_each_entry() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_entry)相同

### `list_for_each_entry_safe(pos, n, head, member)`

#### 描述

从头开始迭代给定类型的链表（支持删除当前链表结点）

#### 参数

**pos**

指向特定类型的结构体的指针

**n**

用于存储临时值的，和pos相同类型的指针

**head**

指向链表头结点的指针(struct List*)

**member**

struct List在pos的结构体中的成员变量名

### `list_prepare_entry(pos, head, member)`

#### 描述

为[list_for_each_entry_continue() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_entry_continue)准备一个'pos'结构体

#### 参数

**pos**

指向特定类型的结构体的，用作迭代起点的指针

**head**

指向要开始迭代的struct List结构体的指针

**member**

struct List在pos的结构体中的成员变量名

### `list_for_each_entry_continue(pos, head, member)` {#_list_for_each_entry_continue}

#### 描述

从指定的位置的【下一个元素开始】,继续迭代给定的链表

#### 参数

**pos**

指向特定类型的结构体的指针。该指针用作迭代的指针。

**head**

指向要开始迭代的struct List结构体的指针

**member**

struct List在pos的结构体中的成员变量名

### `list_for_each_entry_continue_reverse(pos, head, member)`

#### 描述

从指定的位置的【上一个元素开始】,【逆序】迭代给定的链表

#### 参数

与[list_for_each_entry_continue() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_entry_continue)的相同

### `list_for_each_entry_from(pos, head, member)`

#### 描述

从指定的位置开始,继续迭代给定的链表

#### 参数

与[list_for_each_entry_continue() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_entry_continue)的相同

### `list_for_each_entry_safe_continue(pos, n, head, member)` {#_list_for_each_entry_safe_continue}

#### 描述

从指定的位置的【下一个元素开始】,继续迭代给定的链表.（支持删除当前链表结点）

#### 参数

**pos**

指向特定类型的结构体的指针。该指针用作迭代的指针。

**n**

用于存储临时值的，和pos相同类型的指针

**head**

指向要开始迭代的struct List结构体的指针

**member**

struct List在pos的结构体中的成员变量名

### `list_for_each_entry_safe_continue_reverse(pos, n, head, member)`

#### 描述

从指定的位置的【上一个元素开始】,【逆序】迭代给定的链表。（支持删除当前链表结点）

#### 参数

与[list_for_each_entry_safe_continue() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_entry_safe_continue)的相同

### `list_for_each_entry_safe_from(pos, n, head, member)`

#### 描述

从指定的位置开始,继续迭代给定的链表.（支持删除当前链表结点）

#### 参数

与[list_for_each_entry_safe_continue() ](/zh/kernel/core_api/kernel_api.html#_list_for_each_entry_safe_continue)的相同

---

## 基础C函数库

内核编程与应用层编程不同，你将无法使用LibC中的函数来进行编程。为此，内核实现了一些常用的C语言函数，并尽量使其与标准C库中的函数行为相近。值得注意的是，这些函数的行为可能与标准C库函数不同，请在使用时仔细阅读以下文档，这将会为你带来帮助。

### 字符串操作

#### `int strlen(const char *s)`

##### 描述

测量并返回字符串长度。

##### 参数

**src**

源字符串

#### `long strnlen(const char *src, unsigned long maxlen)`

##### 描述

测量并返回字符串长度。当字符串长度大于maxlen时，返回maxlen

##### 参数

**src**

源字符串

**maxlen**

最大长度

#### `long strnlen_user(const char *src, unsigned long maxlen)`

##### 描述

测量并返回字符串长度。当字符串长度大于maxlen时，返回maxlen。

该函数会进行地址空间校验，要求src字符串必须来自用户空间。当源字符串来自内核空间时，将返回0.

##### 参数

**src**

源字符串，地址位于用户空间

**maxlen**

最大长度

#### `char *strncpy(char *dst, const char *src, long count)`

##### 描述

拷贝长度为count个字节的字符串，返回dst字符串

##### 参数

**src**

源字符串

**dst**

目标字符串

**count**

要拷贝的源字符串的长度

#### `char *strcpy(char *dst, const char *src)`

##### 描述

拷贝源字符串，返回dst字符串

##### 参数

**src**

源字符串

**dst**

目标字符串

#### `long strncpy_from_user(char *dst, const char *src, unsigned long size)`

##### 描述

从用户空间拷贝长度为count个字节的字符串到内核空间，返回拷贝的字符串的大小

该函数会对字符串的地址空间进行校验，防止出现地址空间越界的问题。

##### 参数

**src**

源字符串

**dst**

目标字符串

**size**

要拷贝的源字符串的长度

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

第一个字符串

**SecondPart**

第二个字符串

### 内存操作

#### `void *memcpy(void *dst, const void *src, uint64_t size)`

##### 描述

将内存从src处拷贝到dst处。

##### 参数

**dst**

指向目标地址的指针

**src**

指向源地址的指针

**size**

待拷贝的数据大小

#### `void *memmove(void *dst, const void *src, uint64_t size)`

##### 描述

与`memcpy()`类似，但是在源数据区域与目标数据区域之间存在重合时，该函数能防止数据被错误的覆盖。

##### 参数

**dst**

指向目标地址的指针

**src**

指向源地址的指针

**size**

待拷贝的数据大小
