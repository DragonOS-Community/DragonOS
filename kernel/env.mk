include ../env.mk

# 设置编译器
ifeq ($(ARCH), x86_64)
CCPREFIX=x86_64-linux-gnu-
else ifeq ($(ARCH), riscv64)
CCPREFIX=riscv64-linux-gnu-
endif

export CC=$(CCPREFIX)gcc
export LD=$(CCPREFIX)ld
export AS=$(CCPREFIX)as
export NM=$(CCPREFIX)nm
export AR=$(CCPREFIX)ar
export OBJCOPY=$(CCPREFIX)objcopy

export DEBUG=DEBUG

export CFLAGS_DEFINE_ARCH="__$(ARCH)__"

export GLOBAL_CFLAGS := -fno-builtin -fno-stack-protector -D $(CFLAGS_DEFINE_ARCH) -D $(EMULATOR) -O1

ifeq ($(ARCH), x86_64)
GLOBAL_CFLAGS += -mcmodel=large -m64
else ifeq ($(ARCH), riscv64)
GLOBAL_CFLAGS += -mcmodel=medany -march=rv64gc -mabi=lp64d
endif

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif

export RUSTFLAGS := -C link-args=-znostart-stop-gc
export RUSTDOCFLAGS := -C link-args=-znostart-stop-gc
