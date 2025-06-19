:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/process_management/load_binary.md

- Translation time: 2025-05-19 01:41:18

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Loader

## 1. Binary Program Loading

&emsp;&emsp;In this section, you will learn about the principles of the binary loader in DragonOS.

&emsp;&emsp;When DragonOS loads a binary program, it performs a "probe-load" process.

&emsp;&emsp;During the probe phase, DragonOS reads the file header and sequentially calls the probe functions of each binary loader to determine whether the binary program is suitable for that loader. If it is suitable, the loader will be used to load the program.

&emsp;&emsp;During the load phase, DragonOS uses the aforementioned loader to load the program. The loader will map the various segments of the binary program into memory and obtain the entry address of the binary program.

:::{note}
Currently, DragonOS does not support dynamic linking, so all binary programs are statically linked. And only the ELF loader is temporarily supported.
:::
