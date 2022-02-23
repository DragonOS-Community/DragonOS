SUBDIR_ROOTS := . common
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel 
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DIR_LIB=lib
lib_patterns := *.a
LIB_FILES := $(foreach DIR,$(DIR_LIB),$(addprefix $(DIR)/,$(lib_patterns)))

CFLAGS := -mcmodel=large -fno-builtin -m64
ASFLAGS := --64

all: kernel
	objcopy -I elf64-x86-64 -S -R ".comment" -R ".eh_frame" -O elf64-x86-64 kernel ../bin/kernel/kernel.elf
# cp kernel ../bin/kernel/kernel.elf


kernel: head.o entry.o main.o printk.o trap.o mm.o irq.o 8259A.o process.o syscall.o multiboot2.o cpu.o
	ld -b elf64-x86-64 -z muldefs -o kernel head.o exception/entry.o main.o common/printk.o exception/trap.o exception/irq.o mm/mm.o process/process.o syscall/syscall.o driver/multiboot2/multiboot2.o \
	common/cpu.o	\
	driver/interrupt/8259A/8259A.o	\
	-T link.lds

head.o: head.S
	gcc -E head.S > head.s # 预处理
	as $(ASFLAGS) -o head.o head.s
#gcc -mcmodel=large -fno-builtin -m64 -c head.S -o head.o

entry.o: exception/entry.S
	gcc -E exception/entry.S > exception/entry.s
	as $(ASFLAGS) -o exception/entry.o exception/entry.s



main.o: main.c 
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	gcc $(CFLAGS) -c main.c  -o main.o


printk.o: common/printk.c
	gcc $(CFLAGS) -c common/printk.c -o common/printk.o

trap.o:	exception/trap.c
	gcc $(CFLAGS) -c exception/trap.c -o exception/trap.o

irq.o: exception/irq.c
	gcc $(CFLAGS) -c exception/irq.c -o exception/irq.o



mm.o: mm/mm.c
	gcc $(CFLAGS) -c mm/mm.c -o mm/mm.o

process.o: process/process.c
	gcc $(CFLAGS) -c process/process.c -o process/process.o
syscall.o: syscall/syscall.c
	gcc $(CFLAGS) -c syscall/syscall.c -o syscall/syscall.o

multiboot2.o: driver/multiboot2/multiboot2.c 
	gcc $(CFLAGS) -c driver/multiboot2/multiboot2.c  -o driver/multiboot2/multiboot2.o

cpu.o: common/cpu.c 
	gcc $(CFLAGS) -c common/cpu.c -o common/cpu.o


# 驱动程序
8259A.o: driver/interrupt/8259A/8259A.c
	gcc $(CFLAGS) -c driver/interrupt/8259A/8259A.c -o driver/interrupt/8259A/8259A.o
clean: 
	rm -rf $(GARBAGE)