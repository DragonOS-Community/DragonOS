SUBDIR_ROOTS := . common
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ kernel *.a
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

all: kernel
	objcopy -I elf64-x86-64 -S -R  ".eh_frame" -R ".comment" -O binary kernel ../bin/kernel/kernel.bin


kernel: head.o entry.o main.o printk.o trap.o
	ld -b elf64-x86-64 -z muldefs -o kernel head.o exception/entry.o main.o common/printk.o exception/trap.o -T link.lds

head.o: head.S
	gcc -E head.S > head.s # 预处理
	as --64 -o head.o head.s

entry.o: exception/entry.S
	gcc -E exception/entry.S > exception/entry.s
	as --64 -o exception/entry.o exception/entry.s

main.o: main.c 
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	gcc -mcmodel=large -fno-builtin -m64 -c main.c  -o main.o -fno-stack-protector


printk.o: common/printk.c
	gcc -mcmodel=large -fno-builtin -m64 -c common/printk.c -o common/printk.o -fno-stack-protector

trap.o:	exception/trap.c
	gcc -mcmodel=large -fno-builtin -m64 -c exception/trap.c -o exception/trap.o -fno-stack-protector

clean: 
	rm -rf $(GARBAGE)