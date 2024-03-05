ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

# 获得当前git提交的sha1,并截取前8位
GIT_COMMIT_SHA1=$(shell git log -n 1 | head -n 1 | cut -d ' ' -f 2 | cut -c1-8)
CC=$(CROSS_COMPILE)gcc


all: version_header about.c
	$(CC) -static -o about about.c

.PHONY: install clean
install: all
	mv about $(DADK_CURRENT_BUILD_DIR)/about.elf

clean:
	rm about *.o

# 生成版本头文件sys_version.h
version_header: about.c
	@echo "#define DRAGONOS_GIT_COMMIT_SHA1 \"$(GIT_COMMIT_SHA1)\"" > sys_version.h

fmt:
