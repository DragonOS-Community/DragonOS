
ifeq ($(ARCH), )
# ！！！！在这里设置ARCH，可选 x86_64 和 riscv64
# !!!!!!!如果不同时调整这里以及vscode的settings.json，那么自动补全和检查将会失效
export ARCH?=x86_64
endif

ifeq ($(EMULATOR), )
export EMULATOR=__NO_EMULATION__
endif
