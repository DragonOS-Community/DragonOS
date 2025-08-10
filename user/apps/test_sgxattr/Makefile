ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc


all: main.c
	$(CC) -static -o test_sgxattr main.c

.PHONY: install clean
install: all
	mv test_sgxattr $(DADK_CURRENT_BUILD_DIR)/test_sgxattr

clean:
	rm test_sgxattr *.o

fmt:
