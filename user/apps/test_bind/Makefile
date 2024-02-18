CC=$(DragonOS_GCC)/x86_64-elf-gcc
LD=ld
OBJCOPY=objcopy
# 修改这里，把它改为你的relibc的sysroot路径
MUSL_PATH=$(HOME)/opt/x86_64-linux-musl-cross-gcc-9.4.0/x86_64-linux-musl/
CFLAGS=-I $(MUSL_PATH)/include

tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(DADK_BUILD_CACHE_DIR_TEST_BIND_0_1_0)

LIBC_OBJS:=$(shell find $(MUSL_PATH)/lib -name "*.o" | sort )
LIBC_OBJS+=$(MUSL_PATH)/lib/libc.a

all: main.o
	mkdir -p $(tmp_output_dir)
	
	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test_bind  $(shell find . -name "*.o") $(LIBC_OBJS) -T link.lds

	$(OBJCOPY) -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test_bind $(output_dir)/test_bind.elf

	mv $(output_dir)/test_bind.elf $(output_dir)/test_bind
	
main.o: main.c
	$(CC) $(CFLAGS) -c main.c  -o main.o

clean:
	rm -f *.o