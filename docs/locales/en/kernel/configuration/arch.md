:::{note}
**AI Translation Notice**

This document was automatically translated by `Qwen/Qwen3-8B` model, for reference only.

- Source document: kernel/configuration/arch.md

- Translation time: 2025-05-19 01:41:24

- Translation model: `Qwen/Qwen3-8B`

Please report issues via [Community Channel](https://github.com/DragonOS-Community/DragonOS/issues)

:::

# Target Architecture Configuration

## Supported Architectures

- x86_64
- riscv64

## Architecture-Specific Configuration

In order to support the debugging functionality of VSCode, we need to modify the following line in the `.vscode/settings.json` file:
```
    "rust-analyzer.cargo.target": "riscv64gc-unknown-none-elf",
    // "rust-analyzer.cargo.target": "x86_64-unknown-none",
```

If you want to compile for the x86_64 architecture, enable the x86_64 line and comment out the others.
If you want to compile for the riscv64 architecture, enable the riscv64 line and comment out the others.

At the same time, we also need to modify the environment variable configuration in the makefile:

Please modify the following line in the `env.mk` file:
```Makefile
ifeq ($(ARCH), )
# ！！！！在这里设置ARCH，可选x86_64和riscv64
# !!!!!!!如果不同时调整这里以及vscode的settings.json，那么自动补全和检查将会失效
export ARCH=riscv64
endif
```

Please note that changing the architecture requires a recompilation, so please run `make clean` to clean up the compilation results. Then run `make run` to proceed.
