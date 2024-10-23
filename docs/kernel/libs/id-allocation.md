# ID分配

:::{note}
本文作者：龙进 <longjin@DragonOS.org>

2024年9月25日
:::

内核提供了一个名为`IdAllocator`的ID分配器，位于`kernel/crates/ida`中。

它能够分配、释放ID。默认它会自增分配，假如ID大于设定的最大值，它会从最小值开始寻找空闲ID。如果没有空闲的ID，则会分配失败。
