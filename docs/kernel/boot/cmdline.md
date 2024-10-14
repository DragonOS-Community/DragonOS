# 内核启动命令行参数

:::{note}
本文作者: 
- 龙进 <longjin@DragonOS.org>
:::

## 概述

&emsp;&emsp;DragonOS内核启动命令行参数解析模块旨在提供类似Linux的内核启动命令行参数解析支持，以便更灵活地让内核执行不同的行为。该模块允许内核在启动时接收并解析命令行参数，根据参数的不同类型执行相应的回调函数或设置环境变量。

:::{note}
暂时不支持设置回调函数
:::

## 设计方案


### 参数类型

内核启动命令行参数分为三种类型：

- Arg类型
- KV类型
- EarlyKV类型

#### Arg类型

Arg类型的参数在命令行中只有名称，没有值。分为以下两种类型：

- ArgNormal：默认值为`false`，如果命令行中包含这个参数，则会设置为`true`。
- ArgInv：默认值为`true`，如果命令行中包含这个参数，则会设置为`false`。

#### KV类型

KV类型的参数在命令行中表现为`name=value`，`value`按照逗号分隔。内核模块可提供参数的默认值。

#### EarlyKV类型

EarlyKV类型的参数与KV类型类似，但它们在内存管理初始化之前被解析。

### Module标志

Module标志类似于`usbprobe.xxxx`。

### 参数声明
提供宏来声明内核命令行参数。
### procfs支持

:::{note}
TODO: 在`/proc/cmdline`下显示当前内核的启动命令行参数。
:::

## 声明内核启动命令行参数的宏

### Arg类型参数声明
```rust
kernel_cmdline_param_arg!(varname, name, default_bool, inv);
```
- `varname`：参数的变量名
- `name`：参数的名称
- `default_bool`：默认值
- `inv`：是否反转

### KV类型参数声明

```rust
kernel_cmdline_param_kv!(varname, name, default_str);
```

- `varname`：参数的变量名
- `name`：参数的名称
- `default_str`：默认值

### 内存管理初始化之前的KV类型参数声明

```rust
kernel_cmdline_param_early_kv!(varname, name, default_str);
```

- `varname`：参数的变量名
- `name`：参数的名称
- `default_str`：默认值

## 示例

以下示例展示了如何声明和使用KV类型参数：
```rust
kernel_cmdline_param_kv!(ROOTFS_PATH_PARAM, root, "");
if let Some(rootfs_dev_path) = ROOTFS_PATH_PARAM.value_str() {
    .......
} else {
    .......
};
```

### 使用方式

1. 在内核代码中，使用`kernel_cmdline_param_kv!`宏声明所需的KV类型参数。
2. 在内核初始化过程中，通过参数的`value_str()`或者`value_bool()`方法获取参数值。
3. 根据参数值执行相应的操作。

通过以上步骤，开发者可以灵活地使用内核启动命令行参数来控制内核行为。


## TODO

- 支持在`/proc/cmdline`下显示当前内核的启动命令行参数。(需要在procfs重构后)
- 支持设置回调函数，调用回调函数来设置参数值
