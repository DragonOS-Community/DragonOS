ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
CFLAGS := -Wall -O2 -static -lpthread

TARGET := packet_ring_test

all: $(TARGET)

$(TARGET): main.c
	$(CC) $(CFLAGS) $< -o $@

install: all
	@if [ -n "$(DADK_CURRENT_BUILD_DIR)" ]; then \
		echo "Installing $(TARGET) to $(DADK_CURRENT_BUILD_DIR)/"; \
		mv $(TARGET) $(DADK_CURRENT_BUILD_DIR)/; \
	else \
		echo "DADK_CURRENT_BUILD_DIR not set, skipping install"; \
	fi

clean:
	rm -f $(TARGET)

.PHONY: all install clean
