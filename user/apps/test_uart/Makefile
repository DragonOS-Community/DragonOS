ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc

.PHONY: all
all: main.c
	$(CC) -static -o test_uart main.c

.PHONY: install clean
install: all
	mv test_uart $(DADK_CURRENT_BUILD_DIR)/test_uart

clean:
	rm test_uart *.o

fmt:
