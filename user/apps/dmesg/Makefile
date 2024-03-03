ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc


all: main.c dmesg.c
	$(CC) -static -o dmesg main.c dmesg.c

.PHONY: install clean
install: all
	mv dmesg $(DADK_CURRENT_BUILD_DIR)/dmesg

clean:
	rm dmesg *.o

fmt:
