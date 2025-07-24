ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)g++

.PHONY: all
all: main.cpp
	$(CC) -static -o strace main.cpp

.PHONY: install clean
install: all
	mv strace $(DADK_CURRENT_BUILD_DIR)/strace

clean:
	rm strace *.o

fmt:
