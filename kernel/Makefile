SUBDIR_ROOTS := . common
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel 
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DIR_LIB=lib
lib_patterns := *.a
LIB_FILES := $(foreach DIR,$(DIR_LIB),$(addprefix $(DIR)/,$(lib_patterns)))
all: kernel
	objcopy -I elf64-x86-64 -S -R ".comment" -O elf64-x86-64 kernel ../bin/kernel/kernel.elf
# cp kernel ../bin/kernel/kernel.elf


kernel: head.o entry.o main.o printk.o trap.o mm.o irq.o 8259A.o process.o syscall.o multiboot2.o
	ld -b elf64-x86-64 -z muldefs -o kernel head.o exception/entry.o main.o common/printk.o exception/trap.o exception/irq.o exception/8259A.o mm/mm.o process/process.o syscall/syscall.o driver/multiboot2/multiboot2.o \
	-T link.lds

head.o: head.S
	gcc -E head.S > head.s # 预处理
	as --64 -o head.o head.s
#gcc -mcmodel=large -fno-builtin -m64 -c head.S -o head.o

entry.o: exception/entry.S
	gcc -E exception/entry.S > exception/entry.s
	as --64 -o exception/entry.o exception/entry.s



main.o: main.c 
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	gcc -mcmodel=large -fno-builtin -m64 -c main.c  -o main.o


printk.o: common/printk.c
	gcc -mcmodel=large -fno-builtin -m64 -c common/printk.c -o common/printk.o

trap.o:	exception/trap.c
	gcc -mcmodel=large -fno-builtin -m64 -c exception/trap.c -o exception/trap.o

irq.o: exception/irq.c
	gcc -mcmodel=large -fno-builtin -m64 -c exception/irq.c -o exception/irq.o

8259A.o: exception/8259A.c
	gcc -mcmodel=large -fno-builtin -m64 -c exception/8259A.c -o exception/8259A.o

mm.o: mm/mm.c
	gcc -mcmodel=large -fno-builtin -m64 -c mm/mm.c -o mm/mm.o

process.o: process/process.c
	gcc -mcmodel=large -fno-builtin -m64 -c process/process.c -o process/process.o
syscall.o: syscall/syscall.c
	gcc -mcmodel=large -fno-builtin -m64 -c syscall/syscall.c -o syscall/syscall.o

multiboot2.o: driver/multiboot2/multiboot2.c 
	gcc -mcmodel=large -fno-builtin -m64 -c driver/multiboot2/multiboot2.c  -o driver/multiboot2/multiboot2.o

clean: 
	rm -rf $(GARBAGE)