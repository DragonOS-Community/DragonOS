# ID Allocation

::: info Author
Longjin `<longjin@DragonOS.org>`

September 25, 2024
:::


The kernel provides an ID allocator named `IdAllocator`, located in `kernel/crates/ida`.

It is capable of allocating and releasing IDs. By default, it increments to allocate IDs. If the ID exceeds the set maximum value, it will search for an available ID starting from the minimum value. If there are no available IDs, the allocation will fail.
