CC=x86_64-elf-gcc
LD=x86_64-elf-ld
OBJCOPY=x86_64-dragonos-objcopy
# 修改这里，把它改为你的relibc的sysroot路径
RELIBC_SYSROOT=/home/longjin/relibc/sysroot
CFLAGS=-I $(RELIBC_SYSROOT)/usr/include
# 修改这里，把它改为你的DragonOS的根目录
ROOT_PATH=/home/longjin/code/DragonOS
tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(ROOT_PATH)/bin/user

LIBC_OBJS:=$(shell find $(RELIBC_SYSROOT)/usr/lib -name "*.o")
LIBC_OBJS+=$(RELIBC_SYSROOT)/usr/lib/libc.a

all: main.o

	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test  $(shell find . -name "*.o") $(LIBC_OBJS) -T link.lds

	$(OBJCOPY) -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test $(output_dir)/test.elf
main.o: main.c
	$(CC) $(CFLAGS) -c main.c  -o main.o
