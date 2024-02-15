ifeq ($(ARCH), x86_64)
	PREFIX=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	PREFIX=riscv64-linux-musl-
endif

SQLITE_FILENAME=sqlite-amalgamation-3420000
SQLITE3_DIR=$(shell pwd)/$(SQLITE_FILENAME)
CC=$(PREFIX)gcc


out: main.c $(SQLITE3_DIR)/sqlite3.c
	$(CC) -I $(SQLITE3_DIR) -static -o out main.c $(SQLITE3_DIR)/sqlite3.c

.PHONY: install clean download_sqlite3 __download_sqlite3
install: out
	mv out $(DADK_CURRENT_BUILD_DIR)/test_sqlite3

clean:
	rm out *.o

__download_sqlite3:
	@echo "Download sqlite3 from https://mirrors.dragonos.org.cn/pub/third_party/sqlite/$(SQLITE_FILENAME).zip"
	@wget https://mirrors.dragonos.org.cn/pub/third_party/sqlite/$(SQLITE_FILENAME).zip || (@echo "Download sqlite3 failed" && rm $(SQLITE_FILENAME).zip && exit 1)
	@unzip -o $(SQLITE_FILENAME).zip || (@echo "Unzip sqlite3 failed" && exit 1)
	@rm $(SQLITE_FILENAME).zip || (@echo "Remove $(SQLITE_FILENAME).zip failed" && exit 1)

download_sqlite3:
# 如果文件夹不存在，则下载，否则不下载
	@test -d $(SQLITE3_DIR) || $(MAKE) __download_sqlite3
