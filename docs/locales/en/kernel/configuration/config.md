:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/configuration/config.md

- Translation time: 2025-05-19 01:41:17

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Kernel Compilation Configuration Guide

## Principle

&emsp;&emsp;Within the kernel directory, the kernel configuration is set using `kernel.config`. This file is parsed in a manner similar to a TOML file, and then the configuration of each module's `d.config` is parsed to determine the status of features.

## Example

**kernel.config**

```toml
[[module.include]]
name = "init"
path = "src/init/"
enable = "y"
description = ""

[[module.include]]
name = "mm"
path = "src/mm/"
enable = "y"
description = ""
```

- **[[module.include]]:** Adds the module to the include list
- **name:** Module name
- **path:** Module path, where the `d.config` file is located
- **enable:**
  - **y:** Enabled, parse the `d.config` file of the module
  - **n:** Disabled, do not parse
- **description:** Description of the module

**src/mm/d.config**

```toml
[module]
name = "mm"
description = ""

[[module.include]]
name = "allocator"
path = "src/mm/allocator/"
enable = "y"
description = ""

[[module.features]]
name = "mm_debug"
enable = "y"
description = ""
```

- **[module]:** Current module
  - **name:** Name of the current module
  - **description:** Description of the module
- **[[module.include]]:** Modules included in the current module, same as in `kernel.config`
- **[[module.features]]:** Features in the current module
  - **name:** Feature name
  - **enable:** Whether the feature is enabled
    - **y:** Enabled
    - **n:** Disabled
  - **description:** Description of the feature

*The following are the `d.config` files of other modules:*

**src/mm/allocator/d.config**

```toml
[module]
name = "allocator"
description = ""

[[module.features]]
name = "allocator_debug"
enable = "y"
description = ""
```

**src/init/d.config**

```toml
[module]
name = "init"
description = ""

[[module.features]]
name = "init_debug"
enable = "y"
description = ""
```

All features enabled in the `d.config` files of the activated modules will be ultimately generated into the `D.config` file in the kernel directory. That is, `D.config` is the final kernel compilation configuration, as follows:

**D.config**

```
init_debug = y
allocator_debug = y
mm_debug = y
```
