ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
CFLAGS := -Wall -O2 -static -lpthread

# These two legacy programs contain unsupported-as-PASS paths. Their Linux
# conformance coverage is maintained by dunitest and the upstream gVisor suite.
RETIRED_FALSE_POSITIVE_SRCS := test_mount_propagation.c test_pivot_root.c
SRCS := $(filter-out $(RETIRED_FALSE_POSITIVE_SRCS),$(wildcard *.c))
BINS := $(SRCS:.c=)

all: $(BINS)
# 	@echo "src: $(SRCS)"
	@echo "bins: $(BINS)"

# 依赖同目录下所有 .h，任意头文件变更都会触发重编
%: %.c $(wildcard *.h)
	$(CC) $(CFLAGS) $< -o $@

install: all
	@echo "Installing binaries to $(DADK_CURRENT_BUILD_DIR)/"
	mv $(BINS) $(DADK_CURRENT_BUILD_DIR)/

clean:
	rm -f $(BINS) $(RETIRED_FALSE_POSITIVE_SRCS:.c=)

.PHONY: all install clean
