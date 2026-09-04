# DragonOS Kernel Core API

## Circular Linked List Management Functions

Circular linked list is one of the important data structures in the kernel. It is included in `kernel/common/list.h`.

### `void list_init(struct List *list)`

#### Description

Initialize a List structure so that its prev and next pointers point to itself.

#### Parameters

**list**

The List structure to be initialized.

### `void list_add(struct List *entry, struct List *node)`

#### Description

Insert the node after the entry.

#### Parameters

**entry**

An existing node in the circular linked list.

**node**

The node to be inserted.

### `void list_append(struct List *entry, struct List *node)`

#### Description

Insert the node before the entry.

#### Parameters

**entry**

An existing node in the circular linked list.

**node**

The node to be inserted.

### `void list_del(struct List *entry)`

#### Description

Remove the node from the list.

#### Parameters

**entry**

The node to be removed.

### `list_del_init(struct List *entry)`

#### Description

Remove the node from the list and re-initialize the entry using list_init().

#### Parameters

**entry**

The node to be removed.

### `bool list_empty(struct List *entry)`

#### Description

Check if the list is empty.

#### Parameters

**entry**

A node in the list.

### `struct List *list_prev(struct List *entry)`

#### Description

Get the previous node of the entry.

#### Parameters

**entry**

A node in the list.

### `struct List *list_next(struct List *entry)`

#### Description

Get the next node of the entry.

#### Parameters

**entry**

A node in the list.

### `void list_replace(struct List *old, struct List *new)`

#### Description

Replace the old node in the list with the new node.

#### Parameters

**old**

The node to be removed.

**new**

The new node to be inserted into the list.


### `list_entry(ptr, type, member)` {#_list_entry}

#### Description

This macro can get the address of the structure that contains the List pointed to by ptr.

#### Parameters

**ptr**

Pointer to the List structure.

**type**

The type of the structure that contains the List.

**member**

The name of the List structure member in the structure that contains the List.

### `list_first_entry(ptr, type, member)`

#### Description

Get the first element in the list. Please note that this macro requires the list to be non-empty, otherwise it will cause an error.

#### Parameters

Same as [list_entry() ](/kernel/core_api/kernel_api.html#_list_entry)

### `list_first_entry_or_null(ptr, type, member)`

#### Description

Get the first element in the list. If the list is empty, return NULL.

#### Parameters

Same as [list_entry() ](/kernel/core_api/kernel_api.html#_list_entry)

### `list_last_entry(ptr, type, member)`

#### Description

Get the last element in the list. Please note that this macro requires the list to be non-empty, otherwise it will cause an error.

#### Parameters

Same as [list_entry() ](/kernel/core_api/kernel_api.html#_list_entry)

### `list_last_entry_or_full(ptr, type, member)`

#### Description

Get the last element in the list. If the list is empty, return NULL.

#### Parameters

Same as [list_entry() ](/kernel/core_api/kernel_api.html#_list_entry)

### `list_next_entry(pos, member)` {#_list_next_entry}

#### Description

Get the next element in the list.

#### Parameters

**pos**

Pointer to the outer structure.

**member**

The name of the List structure member in the outer structure.

### `list_prev_entry(pos, member)`

#### Description

Get the previous element in the list.

#### Parameters

Same as [list_next_entry() ](/kernel/core_api/kernel_api.html#_list_next_entry)

### `list_for_each(ptr, head)` {#_list_for_each}

#### Description

Traverse the entire list (from front to back).

#### Parameters

**ptr**

Pointer to the List structure.

**head**

Pointer to the head node of the list (struct List*).

### `list_for_each_prev(ptr, head)`

#### Description

Traverse the entire list (from back to front).

#### Parameters

Same as [list_for_each() ](/kernel/core_api/kernel_api.html#_list_for_each)

### `list_for_each_safe(ptr, n, head)` {#_list_for_each_safe}

#### Description

Traverse the entire list from front to back (supports deletion of the current list node).

This macro uses a temporary variable to prevent errors that may occur during iteration if the current ptr node is deleted.

#### Parameters

**ptr**

Pointer to the List structure.

**n**

Pointer to store the temporary value (List type).

**head**

Pointer to the head node of the list (struct List*).

### `list_for_each_prev_safe(ptr, n, head)`

#### Description

Traverse the entire list from back to front (supports deletion of the current list node).

This macro uses a temporary variable to prevent errors that may occur during iteration if the current ptr node is deleted.

#### Parameters

Same as [list_for_each_safe() ](/kernel/core_api/kernel_api.html#_list_for_each_safe)

### `list_for_each_entry(pos, head, member)` {#_list_for_each_entry}

#### Description

Iterate through the list of a given type from the beginning.

#### Parameters

**pos**

Pointer to a structure of the specific type.

**head**

Pointer to the head node of the list (struct List*).

**member**

The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_reverse(pos, head, member)`

#### Description

Iterate through the list of a given type in reverse order.

#### Parameters

Same as [list_for_each_entry() ](/kernel/core_api/kernel_api.html#_list_for_each_entry)

### `list_for_each_entry_safe(pos, n, head, member)`

#### Description

Iterate through the list of a given type from the beginning (supports deletion of the current list node).

#### Parameters

**pos**

Pointer to a structure of the specific type.

**n**

Pointer to store the temporary value (same type as pos).

**head**

Pointer to the head node of the list (struct List*).

**member**

The name of the List member in the structure pointed to by pos.

### `list_prepare_entry(pos, head, member)`

#### Description

Prepare a 'pos' structure for [list_for_each_entry_continue() ](/kernel/core_api/kernel_api.html#_list_for_each_entry_continue).

#### Parameters

**pos**

Pointer to a structure of the specific type, used as the starting point for iteration.

**head**

Pointer to the struct List structure to start iteration from.

**member**

The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_continue(pos, head, member)` {#_list_for_each_entry_continue}

#### Description

Continue iterating through the list from the next element of the specified position.

#### Parameters

**pos**

Pointer to a structure of the specific type. This pointer is used as the iteration pointer.

**head**

Pointer to the struct List structure to start iteration from.

**member**

The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_continue_reverse(pos, head, member)`

#### Description

Iterate through the list in reverse order, starting from the previous element of the specified position.

#### Parameters

Same as [list_for_each_entry_continue() ](/kernel/core_api/kernel_api.html#_list_for_each_entry_continue)

### `list_for_each_entry_from(pos, head, member)`

#### Description

Continue iterating through the list from the specified position.

#### Parameters

Same as [list_for_each_entry_continue() ](/kernel/core_api/kernel_api.html#_list_for_each_entry_continue)

### `list_for_each_entry_safe_continue(pos, n, head, member)` {#_list_for_each_entry_safe_continue}

#### Description

Continue iterating through the list from the next element of the specified position (supports deletion of the current list node).

#### Parameters

**pos**

Pointer to a structure of the specific type. This pointer is used as the iteration pointer.

**n**

Pointer to store the temporary value (same type as pos).

**head**

Pointer to the struct List structure to start iteration from.

**member**

The name of the List member in the structure pointed to by pos.

### `list_for_each_entry_safe_continue_reverse(pos, n, head, member)`

#### Description

Iterate through the list in reverse order, starting from the previous element of the specified position (supports deletion of the current list node).

#### Parameters

Same as [list_for_each_entry_safe_continue() ](/kernel/core_api/kernel_api.html#_list_for_each_entry_safe_continue)

### `list_for_each_entry_safe_from(pos, n, head, member)`

#### Description

Continue iterating through the list from the specified position (supports deletion of the current list node).

#### Parameters

Same as [list_for_each_entry_safe_continue() ](/kernel/core_api/kernel_api.html#_list_for_each_entry_safe_continue)

---

## Basic C Function Library

Kernel programming differs from application layer programming; you will not be able to use functions from LibC. To address this, the kernel implements some commonly used C language functions, trying to make their behavior as close as possible to standard C library functions. It is important to note that the behavior of these functions may differ from standard C library functions, so it is recommended to carefully read the following documentation, which will be helpful to you.

### String Operations

#### `int strlen(const char *s)`

##### Description

Measure and return the length of the string.

##### Parameters

**src**

Source string.

#### `long strnlen(const char *src, unsigned long maxlen)`

##### Description

Measure and return the length of the string. If the string length is greater than maxlen, return maxlen.

##### Parameters

**src**

Source string.

**maxlen**

Maximum length.

#### `long strnlen_user(const char *src, unsigned long maxlen)`

##### Description

Measure and return the length of the string. If the string length is greater than maxlen, return maxlen.

This function performs address space validation, requiring the src string to be from user space. If the source string is from kernel space, it will return 0.

##### Parameters

**src**

Source string, located in user space.

**maxlen**

Maximum length.

#### `char *strncpy(char *dst, const char *src, long count)`

##### Description

Copy a string of count bytes and return the dst string.

##### Parameters

**src**

Source string.

**dst**

Destination string.

**count**

Length of the source string to copy.

#### `char *strcpy(char *dst, const char *src)`

##### Description

Copy the source string and return the dst string.

##### Parameters

**src**

Source string.

**dst**

Destination string.

#### `long strncpy_from_user(char *dst, const char *src, unsigned long size)`

##### Description

Copy a string of count bytes from user space to kernel space, and return the size of the copied string.

This function performs address space validation to prevent address space overflow issues.

##### Parameters

**src**

Source string.

**dst**

Destination string.

**size**

Length of the source string to copy.

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

First string.

**SecondPart**

Second string.

### Memory Operations

#### `void *memcpy(void *dst, const void *src, uint64_t size)`

##### Description

Copy memory from src to dst.

##### Parameters

**dst**

Pointer to the destination address.

**src**

Pointer to the source address.

**size**

Size of data to be copied.

#### `void *memmove(void *dst, const void *src, uint64_t size)`

##### Description

Similar to `memcpy()`, but this function prevents data from being incorrectly overwritten when the source and destination memory regions overlap.

##### Parameters

**dst**

Pointer to the destination address.

**src**

Pointer to the source address.

**size**

Size of data to be copied.
