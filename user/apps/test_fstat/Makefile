CC=$(DragonOS_GCC)/x86_64-elf-gcc
LD=ld
OBJCOPY=objcopy
# 修改这里，把它改为你的relibc的sysroot路径
RELIBC_OPT=$(DADK_BUILD_CACHE_DIR_RELIBC_0_1_0)
CFLAGS=-I $(RELIBC_OPT)/include -D__dragonos__

tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(DADK_BUILD_CACHE_DIR_TEST_FSTAT_0_1_0)

LIBC_OBJS:=$(shell find $(RELIBC_OPT)/lib -name "*.o" | sort )
LIBC_OBJS+=$(RELIBC_OPT)/lib/libc.a

all: main.o
	mkdir -p $(tmp_output_dir)
	
	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test_fstat  $(shell find . -name "*.o") $(LIBC_OBJS) -T link.lds

	$(OBJCOPY) -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test_fstat $(output_dir)/test_fstat.elf
	
	mv $(output_dir)/test_fstat.elf $(output_dir)/test_fstat
main.o: main.c
	$(CC) $(CFLAGS) -c main.c  -o main.o

clean:
	rm -f *.o