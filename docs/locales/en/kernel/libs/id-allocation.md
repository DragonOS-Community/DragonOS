:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/libs/id-allocation.md

- Translation time: 2025-05-19 01:41:12

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# ID Allocation

:::{note}
Author: Longjin <longjin@DragonOS.org>

September 25, 2024
:::

The kernel provides an ID allocator named `IdAllocator`, located in `kernel/crates/ida`.

It is capable of allocating and releasing IDs. By default, it increments to allocate IDs. If the ID exceeds the set maximum value, it will search for an available ID starting from the minimum value. If there are no available IDs, the allocation will fail.
