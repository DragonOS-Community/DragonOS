# 导入环境变量
include ./env.mk

# export ARCH ?= x86_64
# 如果是x86_64, 则使用x86_64-unknown-none.json作为target
ifeq ($(ARCH), x86_64)
	export TARGET_JSON=arch/x86_64/x86_64-unknown-none.json
else ifeq ($(ARCH), riscv64)
	export TARGET_JSON=riscv64gc-unknown-none-elf
endif

export CARGO_ZBUILD=-Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem

.PHONY: ECHO
ECHO:
	@echo "$@"

all: 
	$(MAKE) -C src all ARCH=$(ARCH) || (sh -c "echo 内核编译失败" && exit 1)
	

clean:
	rm -f Cargo.lock
	$(MAKE) -C src clean ARCH=$(ARCH)

.PHONY: fmt
fmt:
	RUSTFLAGS="$(RUSTFLAGS)" cargo fmt --all $(FMT_CHECK)
ifeq ($(ARCH), x86_64)
	RUSTFLAGS="$(RUSTFLAGS)" cargo clippy --all-features
endif


.PHONY: check
check: ECHO
# @echo "Checking kernel... ARCH=$(ARCH)"
# @exit 1
ifeq ($(ARCH), x86_64)
	RUSTFLAGS="$(RUSTFLAGS)" cargo +nightly-2024-07-23 check --workspace $(CARGO_ZBUILD) --message-format=json --target ./src/$(TARGET_JSON)
else ifeq ($(ARCH), riscv64)
	RUSTFLAGS="$(RUSTFLAGS)" cargo +nightly-2024-07-23 check --workspace $(CARGO_ZBUILD) --message-format=json --target $(TARGET_JSON)
endif

test:
# 测试内核库
	RUSTFLAGS="$(RUSTFLAGS)" cargo +nightly-2024-07-23 test --workspace --exclude dragonos_kernel

