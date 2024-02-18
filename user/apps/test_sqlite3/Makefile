CC=$(DragonOS_GCC)/x86_64-elf-gcc
LD=ld
OBJCOPY=objcopy
SQLITE_FILENAME=sqlite-amalgamation-3420000
SQLITE3_DIR=$(shell pwd)/$(SQLITE_FILENAME)

RELIBC_OPT=$(DADK_BUILD_CACHE_DIR_RELIBC_0_1_0)
CFLAGS=-I $(RELIBC_OPT)/include -I $(SQLITE3_DIR) -D__dragonos__ -DSQLITE_THREADSAFE=0 -DSQLITE_OMIT_FLOATING_POINT -DSQLITE_OMIT_LOAD_EXTENSION -DSQLITE_DEBUG

tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(DADK_BUILD_CACHE_DIR_TEST_SQLITE3_3_42_0)

LIBC_OBJS:=$(shell find $(RELIBC_OPT)/lib -name "*.o" | sort )
LIBC_OBJS+=$(RELIBC_OPT)/lib/libc.a

.PHONY: all clean download_sqlite3 __download_sqlite3


all: main.o sqlite3.o
	mkdir -p $(tmp_output_dir)
	
	$(LD) -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/test_sqlite3  $(shell find . -name "*.o") $(LIBC_OBJS)

	$(OBJCOPY) -I elf64-x86-64 -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/test_sqlite3 $(output_dir)/test_sqlite3.elf
	mv $(output_dir)/test_sqlite3.elf $(output_dir)/test_sqlite3

main.o: main.c
	$(CC) $(CFLAGS) -c main.c  -o main.o

sqlite3.o: $(SQLITE3_DIR)/sqlite3.c
	$(CC) $(CFLAGS) -c $(SQLITE3_DIR)/sqlite3.c -o sqlite3.o

__download_sqlite3:
	@echo "Download sqlite3 from https://mirrors.dragonos.org.cn/pub/third_party/sqlite/$(SQLITE_FILENAME).zip"
	@wget https://mirrors.dragonos.org.cn/pub/third_party/sqlite/$(SQLITE_FILENAME).zip || (@echo "Download sqlite3 failed" && rm $(SQLITE_FILENAME).zip && exit 1)
	@unzip -o $(SQLITE_FILENAME).zip || (@echo "Unzip sqlite3 failed" && exit 1)
	@rm $(SQLITE_FILENAME).zip || (@echo "Remove $(SQLITE_FILENAME).zip failed" && exit 1)

download_sqlite3:
# 如果文件夹不存在，则下载，否则不下载
	@test -d $(SQLITE3_DIR) || $(MAKE) __download_sqlite3

clean:
	rm -f *.o
