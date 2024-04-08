ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc


all:
	$(CC) -static -o test_pty test_pty.c

.PHONY: install clean
install: all
	mv test_pty $(DADK_CURRENT_BUILD_DIR)/test_pty

clean:
	rm test_pty *.o

fmt:
