ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
BINFILE=test-fork-wait

all: main.c
	$(CC) -static -o $(BINFILE) main.c

.PHONY: install clean
install: all
	mv $(BINFILE) $(DADK_CURRENT_BUILD_DIR)/

clean:
	rm $(BINFILE) *.o

fmt:
