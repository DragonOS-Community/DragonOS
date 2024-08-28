include ../env.mk

# 设置编译器
ifeq ($(ARCH), x86_64)

# 如果 x86_64时，DragonOS_GCC 为空，那么设置为默认值
export DragonOS_GCC?=$(HOME)/opt/dragonos-gcc/gcc-x86_64-unknown-none/bin

export CC=$(DragonOS_GCC)/x86_64-elf-gcc
export LD=ld
export AS=$(DragonOS_GCC)/x86_64-elf-as
export NM=$(DragonOS_GCC)/x86_64-elf-nm
export AR=$(DragonOS_GCC)/x86_64-elf-ar
export OBJCOPY=$(DragonOS_GCC)/x86_64-elf-objcopy

else ifeq ($(ARCH), riscv64)

export CC=riscv64-unknown-elf-gcc
# binutils版本需要>=2.38
# 而ubuntu的unknown-elf的版本比较旧，所以使用了riscv64-linux-gnu-ld
export LD=riscv64-linux-gnu-ld
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
GLOBAL_CFLAGS += -mcmodel=medany -march=rv64gc -mabi=lp64d
endif

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif

export RUSTFLAGS := -C link-args=-znostart-stop-gc
export RUSTDOCFLAGS := -C link-args=-znostart-stop-gc