ifeq ($(ARCH), x86_64)
	PREFIX=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	PREFIX=riscv64-linux-musl-
endif

# 获得当前git提交的sha1,并截取前8位
GIT_COMMIT_SHA1=$(shell git log -n 1 | head -n 1 | cut -d ' ' -f 2 | cut -c1-8)
CC=$(PREFIX)gcc


out: version_header about.c
	$(CC) -static -o out about.c

.PHONY: install clean
install: out
	mv out $(DADK_CURRENT_BUILD_DIR)/about

clean:
	rm out *.o

# 生成版本头文件sys_version.h
version_header: about.c
	@echo "#define DRAGONOS_GIT_COMMIT_SHA1 \"$(GIT_COMMIT_SHA1)\"" > sys_version.h
