ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc

.PHONY: all
all: main.c
	$(CC) -static -o test_poll main.c

.PHONY: install clean
install: all
	mv test_poll $(DADK_CURRENT_BUILD_DIR)/test_poll

clean:
	rm test_poll *.o

fmt:
