:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/core_api/kernel_api.md

- Translation time: 2025-05-19 01:43:39

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# DragonOS Kernel Core API

## Circular Linked List Management Functions

&emsp;&emsp;Circular linked list is one of the important data structures in the kernel. It is included in `kernel/common/list.h`.

### `void list_init(struct List *list)`

#### Description

&emsp;&emsp;Initialize a List structure so that its prev and next pointers point to itself.

#### Parameters

**list**

&emsp;&emsp;The List structure to be initialized.

### `void list_add(struct List *entry, struct List *node)`

#### Description

&emsp;&emsp;Insert the node after the entry.

#### Parameters

**entry**

&emsp;&emsp;An existing node in the circular linked list.

**node**

&emsp;&emsp;The node to be inserted.

### `void list_append(struct List *entry, struct List *node)`

#### Description

&emsp;&emsp;Insert the node before the entry.

#### Parameters

**entry**

&emsp;&emsp;An existing node in the circular linked list.

**node**

&emsp;&emsp;The node to be inserted.

### `void list_del(struct List *entry)`

#### Description

&emsp;&emsp;Remove the node from the list.

#### Parameters

**entry**

&emsp;&emsp;The node to be removed.

### `list_del_init(struct List *entry)`

#### Description

&emsp;&emsp;Remove the node from the list and re-initialize the entry using list_init().

#### Parameters

**entry**

&emsp;&emsp;The node to be removed.

### `bool list_empty(struct List *entry)`

#### Description

&emsp;&emsp;Check if the list is empty.

#### Parameters

**entry**

&emsp;&emsp;A node in the list.

### `struct List *list_prev(struct List *entry)`

#### Description

&emsp;&emsp;Get the previous node of the entry.

#### Parameters

**entry**

&emsp;&emsp;A node in the list.

### `struct List *list_next(struct List *entry)`

#### Description

&emsp;&emsp;Get the next node of the entry.

#### Parameters

**entry**

&emsp;&emsp;A node in the list.

### `void list_replace(struct List *old, struct List *new)`

#### Description

&emsp;&emsp;Replace the old node in the list with the new node.

#### Parameters

**old**

&emsp;&emsp;The node to be removed.

**new**

&emsp;&emsp;The new node to be inserted into the list.

(_translated_label___list_entry_en)=

### `list_entry(ptr, type, member)`

#### Description

&emsp;&emsp;This macro can get the address of the structure that contains the List pointed to by ptr.

#### Parameters

**ptr**

&emsp;&emsp;Pointer to the List structure.

**type**

&emsp;&emsp;The type of the structure that contains the List.

**member**

&emsp;&emsp;The name of the List structure member in the structure that contains the List.

### `list_first_entry(ptr, type, member)`

#### Description

&emsp;&emsp;Get the first element in the list. Please note that this macro requires the list to be non-empty, otherwise it will cause an error.

#### Parameters

&emsp;&emsp;Same as {ref}`list_entry() <_list_entry>`

### `list_first_entry_or_null(ptr, type, member)`

#### Description

&emsp;&emsp;Get the first element in the list. If the list is empty, return NULL.

#### Parameters

&emsp;&emsp;Same as {ref}`list_entry() <_list_entry>`

### `list_last_entry(ptr, type, member)`

#### Description

&emsp;&emsp;Get the last element in the list. Please note that this macro requires the list to be non-empty, otherwise it will cause an error.

#### Parameters

&emsp;&emsp;Same as {ref}`list_entry() <_list_entry>`

### `list_last_entry_or_full(ptr, type, member)`

#### Description

&emsp;&emsp;Get the last element in the list. If the list is empty, return NULL.

#### Parameters

&emsp;&emsp;Same as {ref}`list_entry() <_list_entry>`

(_translated_label___list_next_entry_en)=
### `list_next_entry(pos, member)`

#### Description

&emsp;&emsp;Get the next element in the list.

#### Parameters

**pos**

&emsp;&emsp;Pointer to the outer structure.

**member**

&emsp;&emsp;The name of the List structure member in the outer structure.

### `list_prev_entry(pos, member)`

#### Description

&emsp;&emsp;Get the previous element in the list.

#### Parameters

&emsp;&emsp;Same as {ref}`list_next_entry() <_list_next_entry>`

(_translated_label___list_for_each_en)=
### `list_for_each(ptr, head)`

#### Description

&emsp;&emsp;Traverse the entire list (from front to back).

#### Parameters

**ptr**

&emsp;&emsp;Pointer to the List structure.

**head**

&emsp;&emsp;Pointer to the head node of the list (struct List*).

### `list_for_each_prev(ptr, head)`

#### Description

&emsp;&emsp;Traverse the entire list (from back to front).

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each() <_list_for_each>`

(_translated_label___list_for_each_safe_en)=
### `list_for_each_safe(ptr, n, head)`

#### Description

&emsp;&emsp;Traverse the entire list from front to back (supports deletion of the current list node).

&emsp;&emsp;This macro uses a temporary variable to prevent errors that may occur during iteration if the current ptr node is deleted.

#### Parameters

**ptr**

&emsp;&emsp;Pointer to the List structure.

**n**

&emsp;&emsp;Pointer to store the temporary value (List type).

**head**

&emsp;&emsp;Pointer to the head node of the list (struct List*).

### `list_for_each_prev_safe(ptr, n, head)`

#### Description

&emsp;&emsp;Traverse the entire list from back to front (supports deletion of the current list node).

&emsp;&emsp;This macro uses a temporary variable to prevent errors that may occur during iteration if the current ptr node is deleted.

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each_safe() <_list_for_each_safe>`

(_translated_label___list_for_each_entry_en)=
### `list_for_each_entry(pos, head, member)`

#### Description

&emsp;&emsp;Iterate through the list of a given type from the beginning.

#### Parameters

**pos**

&emsp;&emsp;Pointer to a structure of the specific type.

**head**

&emsp;&emsp;Pointer to the head node of the list (struct List*).

**member**

&emsp;&emsp;The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_reverse(pos, head, member)`

#### Description

&emsp;&emsp;Iterate through the list of a given type in reverse order.

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each_entry() <_list_for_each_entry>`

### `list_for_each_entry_safe(pos, n, head, member)`

#### Description

&emsp;&emsp;Iterate through the list of a given type from the beginning (supports deletion of the current list node).

#### Parameters

**pos**

&emsp;&emsp;Pointer to a structure of the specific type.

**n**

&emsp;&emsp;Pointer to store the temporary value (same type as pos).

**head**

&emsp;&emsp;Pointer to the head node of the list (struct List*).

**member**

&emsp;&emsp;The name of the List member in the structure pointed to by pos.

### `list_prepare_entry(pos, head, member)`

#### Description

&emsp;&emsp;Prepare a 'pos' structure for {ref}`list_for_each_entry_continue() <_list_for_each_entry_continue>`.

#### Parameters

**pos**

&emsp;&emsp;Pointer to a structure of the specific type, used as the starting point for iteration.

**head**

&emsp;&emsp;Pointer to the struct List structure to start iteration from.

**member**

&emsp;&emsp;The name of the List member in the structure pointed to by pos.

(_translated_label___list_for_each_entry_continue_en)=
### `list_for_each_entry_continue(pos, head, member)`

#### Description

&emsp;&emsp;Continue iterating through the list from the next element of the specified position.

#### Parameters

**pos**

&emsp;&emsp;Pointer to a structure of the specific type. This pointer is used as the iteration pointer.

**head**

&emsp;&emsp;Pointer to the struct List structure to start iteration from.

**member**

&emsp;&emsp;The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_continue_reverse(pos, head, member)`

#### Description

&emsp;&emsp;Iterate through the list in reverse order, starting from the previous element of the specified position.

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each_entry_continue() <_list_for_each_entry_continue>`

### `list_for_each_entry_from(pos, head, member)`

#### Description

&emsp;&emsp;Continue iterating through the list from the specified position.

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each_entry_continue() <_list_for_each_entry_continue>`

(_translated_label___list_for_each_entry_safe_continue_en)=
### `list_for_each_entry_safe_continue(pos, n, head, member)`

#### Description

&emsp;&emsp;Continue iterating through the list from the next element of the specified position (supports deletion of the current list node).

#### Parameters

**pos**

&emsp;&emsp;Pointer to a structure of the specific type. This pointer is used as the iteration pointer.

**n**

&emsp;&emsp;Pointer to store the temporary value (same type as pos).

**head**

&emsp;&emsp;Pointer to the struct List structure to start iteration from.

**member**

&emsp;&emsp;The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_safe_continue_reverse(pos, n, head, member)`

#### Description

&emsp;&emsp;Iterate through the list in reverse order, starting from the previous element of the specified position (supports deletion of the current list node).

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each_entry_safe_continue() <_list_for_each_entry_safe_continue>`

### `list_for_each_entry_safe_from(pos, n, head, member)`

#### Description

&emsp;&emsp;Continue iterating through the list from the specified position (supports deletion of the current list node).

#### Parameters

&emsp;&emsp;Same as {ref}`list_for_each_entry_safe_continue() <_list_for_each_entry_safe_continue>`

---

## Basic C Function Library

&emsp;&emsp;Kernel programming differs from application layer programming; you will not be able to use functions from LibC. To address this, the kernel implements some commonly used C language functions, trying to make their behavior as close as possible to standard C library functions. It is important to note that the behavior of these functions may differ from standard C library functions, so it is recommended to carefully read the following documentation, which will be helpful to you.

### String Operations

#### `int strlen(const char *s)`

##### Description

&emsp;&emsp;Measure and return the length of the string.

##### Parameters

**src**

&emsp;&emsp;Source string.

#### `long strnlen(const char *src, unsigned long maxlen)`

##### Description

&emsp;&emsp;Measure and return the length of the string. If the string length is greater than maxlen, return maxlen.

##### Parameters

**src**

&emsp;&emsp;Source string.

**maxlen**

&emsp;&emsp;Maximum length.

#### `long strnlen_user(const char *src, unsigned long maxlen)`

##### Description

&emsp;&emsp;Measure and return the length of the string. If the string length is greater than maxlen, return maxlen.

&emsp;&emsp;This function performs address space validation, requiring the src string to be from user space. If the source string is from kernel space, it will return 0.

##### Parameters

**src**

&emsp;&emsp;Source string, located in user space.

**maxlen**

&emsp;&emsp;Maximum length.

#### `char *strncpy(char *dst, const char *src, long count)`

##### Description

&emsp;&emsp;Copy a string of count bytes and return the dst string.

##### Parameters

**src**

&emsp;&emsp;Source string.

**dst**

&emsp;&emsp;Destination string.

**count**

&emsp;&emsp;Length of the source string to copy.

#### `char *strcpy(char *dst, const char *src)`

##### Description

&emsp;&emsp;Copy the source string and return the dst string.

##### Parameters

**src**

&emsp;&emsp;Source string.

**dst**

&emsp;&emsp;Destination string.

#### `long strncpy_from_user(char *dst, const char *src, unsigned long size)`

##### Description

&emsp;&emsp;Copy a string of count bytes from user space to kernel space, and return the size of the copied string.

&emsp;&emsp;This function performs address space validation to prevent address space overflow issues.

##### Parameters

**src**

&emsp;&emsp;Source string.

**dst**

&emsp;&emsp;Destination string.

**size**

&emsp;&emsp;Length of the source string to copy.

#### `int strcmp(char *FirstPart, char *SecondPart)`

##### Description

  Compare the sizes of two strings.

***Return Value***

| Situation                      | Return Value |
| ----------------------- | --- |
| FirstPart == SecondPart | 0   |
| FirstPart > SecondPart  | 1   |
| FirstPart < SecondPart  | -1  |

##### Parameters

**FirstPart**

&emsp;&emsp;First string.

**SecondPart**

&emsp;&emsp;Second string.

### Memory Operations

#### `void *memcpy(void *dst, const void *src, uint64_t size)`

##### Description

&emsp;&emsp;Copy memory from src to dst.

##### Parameters

**dst**

&emsp;&emsp;Pointer to the destination address.

**src**

&emsp;&emsp;Pointer to the source address.

**size**

&emsp;&emsp;Size of data to be copied.

#### `void *memmove(void *dst, const void *src, uint64_t size)`

##### Description

&emsp;&emsp;Similar to `memcpy()`, but this function prevents data from being incorrectly overwritten when the source and destination memory regions overlap.

##### Parameters

**dst**

&emsp;&emsp;Pointer to the destination address.

**src**

&emsp;&emsp;Pointer to the source address.

**size**

&emsp;&emsp;Size of data to be copied.
