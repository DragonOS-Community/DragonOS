# 目标架构配置

## 支持的架构

- x86_64
- riscv64

## 架构相关配置

为了能支持vscode的调试功能，我们需要修改`.vscode/settings.json`文件的以下行：
```
    "rust-analyzer.cargo.target": "riscv64imac-unknown-none-elf",
    // "rust-analyzer.cargo.target": "x86_64-unknown-none",
```

如果想要为x86_64架构编译，请启用x86_64那一行，注释掉其它的。
如果想要为riscv64架构编译，请启用riscv64那一行，注释掉其它的。


同时，我们还需要修改makefile的环境变量配置：

请修改`env.mk`文件的以下行：
```Makefile
ifeq ($(ARCH), )
# ！！！！在这里设置ARCH，可选x86_64和riscv64
# !!!!!!!如果不同时调整这里以及vscode的settings.json，那么自动补全和检查将会失效
export ARCH=riscv64
endif
```

请注意，更换架构需要重新编译，因此请运行`make clean`清理编译结果。然后再运行`make run`即可。
