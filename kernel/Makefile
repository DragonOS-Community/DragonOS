
all: kernel
	objcopy -I elf64-x86-64 -S -R  ".eh_frame" -R ".comment" -O binary kernel ../bin/kernel/kernel.bin


kernel: head.o main.o
	ld -b elf64-x86-64 -o kernel head.o main.o -T link.lds

main.o: main.c
# -fno-builtin: 不使用C语言内建函数
# The -m64 option sets int to 32bits and long and pointer to 64 bits and generates code for AMD’s x86-64 architecture.
	gcc -mcmodel=large -fno-builtin -m64 -c main.c

head.o: head.S
	gcc -E head.S > head.s # 预处理
	as --64 -o head.o head.s

clean:
	rm -rf *.o *.s~ *.s *.S~ *.c~ *.h~ kernel