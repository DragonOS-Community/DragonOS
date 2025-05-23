:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/boot/cmdline.md

- Translation time: 2025-05-19 01:41:48

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Kernel Boot Command Line Parameters

:::{note}
Author:  
- Longjin <longjin@DragonOS.org>
:::

## Overview

&emsp;&emsp;The DragonOS kernel boot command line parameter parsing module aims to provide support for parsing kernel boot command line parameters similar to Linux, enabling more flexible behavior for the kernel. This module allows the kernel to receive and parse command line parameters at boot time, and execute corresponding callback functions or set environment variables based on the type of parameters.

:::{note}
Callback functions are not supported temporarily.
:::

## Design

### Parameter Types

Kernel boot command line parameters are divided into three types:

- Arg type
- KV type
- EarlyKV type

#### Arg Type

Arg type parameters have only a name and no value. They are divided into the following two types:

- ArgNormal: The default value is `false`. If the parameter is present in the command line, it will be set to `true`.
- ArgInv: The default value is `true`. If the parameter is present in the command line, it will be set to `false`.

#### KV Type

KV type parameters are represented in the command line as `name=value`, `value` separated by commas. Kernel modules can provide default values for these parameters.

#### EarlyKV Type

EarlyKV type parameters are similar to KV type parameters, but they are parsed before memory management initialization.

### Module Flags

Module flags are similar to `usbprobe.xxxx`.

### Parameter Declaration

Provides macros to declare kernel command line parameters.
### procfs Support

:::{note}
TODO: Display the current kernel's boot command line parameters under `/proc/cmdline`.
:::

## Macros for Declaring Kernel Boot Command Line Parameters

### Arg Type Parameter Declaration
```rust
kernel_cmdline_param_arg!(varname, name, default_bool, inv);
```
- `varname`: The variable name of the parameter
- `name`: The name of the parameter
- `default_bool`: The default value
- `inv`: Whether to invert

### KV Type Parameter Declaration

```rust
kernel_cmdline_param_kv!(varname, name, default_str);
```

- `varname`: The variable name of the parameter
- `name`: The name of the parameter
- `default_str`: The default value

### KV Type Parameter Declaration Before Memory Management Initialization

```rust
kernel_cmdline_param_early_kv!(varname, name, default_str);
```

- `varname`: The variable name of the parameter
- `name`: The name of the parameter
- `default_str`: The default value

## Example

The following example demonstrates how to declare and use KV type parameters:
```rust
kernel_cmdline_param_kv!(ROOTFS_PATH_PARAM, root, "");
if let Some(rootfs_dev_path) = ROOTFS_PATH_PARAM.value_str() {
    .......
} else {
    .......
};
```

### Usage

1. In the kernel code, use the `kernel_cmdline_param_kv!` macro to declare the required KV type parameters.
2. During kernel initialization, retrieve the parameter value through the `value_str()` or `value_bool()` method of the parameter.
3. Execute corresponding operations based on the parameter value.

By following these steps, developers can flexibly use kernel boot command line parameters to control kernel behavior.

## TODO

- Support displaying the current kernel's boot command line parameters under `/proc/cmdline` (requires procfs refactoring)
- Support setting callback functions to set parameter values
