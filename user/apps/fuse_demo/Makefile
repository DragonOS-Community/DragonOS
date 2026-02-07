ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
CFLAGS := -Wall -O2 -static -lpthread

SRCS := fuse_demo.c
BINS := $(SRCS:.c=)

all: $(BINS)
	@echo "bins: $(BINS)"

%: %.c fuse_test_simplefs.h
	$(CC) $(CFLAGS) $< -o $@

install: all
	@echo "Installing binaries to $(DADK_CURRENT_BUILD_DIR)/"
	mv $(BINS) $(DADK_CURRENT_BUILD_DIR)/

clean:
	rm -f $(BINS)

.PHONY: all install clean
