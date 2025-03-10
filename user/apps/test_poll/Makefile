ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

BIN_NAME=test_poll
CC=$(CROSS_COMPILE)gcc

.PHONY: all
all: main.c
	$(CC) -static -o $(BIN_NAME) main.c

.PHONY: install clean
install: all
	mv $(BIN_NAME) $(DADK_CURRENT_BUILD_DIR)/$(BIN_NAME)

clean:
	rm $(BIN_NAME) *.o

fmt:
