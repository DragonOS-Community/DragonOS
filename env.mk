
ifeq ($(ARCH), )
# ！！！！在这里设置ARCH，可选x86_64和riscv64
# !!!!!!!如果不同时调整这里以及vscode的settings.json，那么自动补全和检查将会失效
export ARCH?=x86_64
endif

ifeq ($(EMULATOR), )
export EMULATOR=__NO_EMULATION__
endif

# 设置编译器
ifeq ($(ARCH), x86_64)

export CC=$(DragonOS_GCC)/x86_64-elf-gcc
export LD=ld
export AS=$(DragonOS_GCC)/x86_64-elf-as
export NM=$(DragonOS_GCC)/x86_64-elf-nm
export AR=$(DragonOS_GCC)/x86_64-elf-ar
export OBJCOPY=$(DragonOS_GCC)/x86_64-elf-objcopy

else ifeq ($(ARCH), riscv64)

export CC=riscv64-unknown-elf-gcc
export LD=riscv64-unknown-elf-ld
export AS=riscv64-unknown-elf-as
export NM=riscv64-unknown-elf-nm
export AR=riscv64-unknown-elf-ar
export OBJCOPY=riscv64-unknown-elf-objcopy

endif


export DEBUG=DEBUG

export CFLAGS_DEFINE_ARCH="__$(ARCH)__"

export GLOBAL_CFLAGS := -fno-builtin -fno-stack-protector -D $(CFLAGS_DEFINE_ARCH) -D $(EMULATOR) -O1

ifeq ($(ARCH), x86_64)
GLOBAL_CFLAGS += -mcmodel=large -m64
else ifeq ($(ARCH), riscv64)
GLOBAL_CFLAGS += -mcmodel=medany -march=rv64imac -mabi=lp64
endif

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif