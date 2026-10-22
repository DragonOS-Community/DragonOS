ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc
# -no-pie: keep runtime addresses equal to ELF vaddrs so the uprobe test can
# resolve its own symbol file offset without a load-base lookup.
CFLAGS := -Wall -O2 -static -no-pie

BIN := test_ebpf_smoke

all: $(BIN)

$(BIN): test_ebpf_smoke.c
	$(CC) $(CFLAGS) $< -o $@

install: all
ifdef DADK_CURRENT_BUILD_DIR
	mv $(BIN) $(DADK_CURRENT_BUILD_DIR)/
else
	mkdir -p ./install && mv $(BIN) ./install/
endif

clean:
	rm -f $(BIN)

.PHONY: all install clean
