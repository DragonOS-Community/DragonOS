:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: userland/appdev/c-cpp-quick-start.md

- Translation time: 2025-05-19 01:41:49

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Developing C/C++ Applications for DragonOS

## Compilation Environment

&emsp;&emsp;DragonOS has partial binary compatibility with Linux, so you can use the musl-gcc compiler from Linux. However, since DragonOS does not currently support dynamic linking, you need to add the compilation parameter `-static`.

For example, you can use the following command:
```shell
musl-gcc -static -o hello hello.c
```
to compile a hello.c file.

When porting existing programs, you may need to configure `CFLAGS`, `LDFLAGS`, and `CPPFLAGS` to ensure correct compilation. Please refer to the actual requirements.

## Configuring DADK

Please refer to: [Quick Start | DADK](https://docs.dragonos.org.cn/p/dadk/user-manual/quickstart.html)
