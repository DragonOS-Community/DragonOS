ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
CFLAGS := -Wall -O2 -static -lpthread

SRCS := $(wildcard *.c)
BINS := $(SRCS:.c=)



$(C_TARGETS): %.o: %.c
	$(CC) -c $< -o $@

all: $(BINS)
# 	@echo "src: $(SRCS)"
	@echo "bins: $(BINS)"

%: %.c
	$(CC) $(CFLAGS)  $< -o $@


install: all
	@echo "Installing binaries to $(DADK_CURRENT_BUILD_DIR)/"
	mv $(BINS) $(DADK_CURRENT_BUILD_DIR)/


clean:
	rm -f $(BINS)


.PHONY: all install clean
