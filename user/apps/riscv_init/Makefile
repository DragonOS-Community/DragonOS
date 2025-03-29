ifeq ($(ARCH), x86_64)
	CROSS_COMPILE=x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE=riscv64-linux-musl-
endif

CC=$(CROSS_COMPILE)gcc


.PHONY: all
all: main.c
# $(CC) -static -o init main.c
	$(MAKE) -C riscv_rust_init ARCH=$(ARCH) install

.PHONY: install clean
install: all
	$(MAKE) -C riscv_rust_init ARCH=$(ARCH) install
# mv init $(DADK_CURRENT_BUILD_DIR)/init


clean:
	rm init *.o
	$(MAKE) -C riscv_rust_init ARCH=$(ARCH) clean

fmt:
