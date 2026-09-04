# Loader

## 1. Binary Program Loading

In this section, you will learn about the principles of the binary loader in DragonOS.

When DragonOS loads a binary program, it performs a "probe-load" process.

During the probe phase, DragonOS reads the file header and sequentially calls the probe functions of each binary loader to determine whether the binary program is suitable for that loader. If it is suitable, the loader will be used to load the program.

During the load phase, DragonOS uses the aforementioned loader to load the program. The loader will map the various segments of the binary program into memory and obtain the entry address of the binary program.

::: info
Currently, DragonOS does not support dynamic linking, so all binary programs are statically linked. And only the ELF loader is temporarily supported.
:::

