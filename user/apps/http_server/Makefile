ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc

.PHONY: all
all: main.c
	$(CC) -static -o http_server main.c

.PHONY: install clean
install: all
	mv http_server $(DADK_CURRENT_BUILD_DIR)/http_server

clean:
	rm http_server *.o

fmt:
