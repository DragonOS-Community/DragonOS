# 导入环境变量
include ../env.mk

export ARCH ?= x86_64
# 如果是x86_64, 则使用x86_64-unknown-none.json作为target
ifeq ($(ARCH), x86_64)
	export TARGET_JSON=arch/x86_64/x86_64-unknown-none.json
else ifeq ($(ARCH), riscv64)
	export TARGET_JSON=arch/riscv64/riscv64imac-unknown-none-elf.json
endif

export CARGO_ZBUILD=-Z build-std=core,alloc,compiler_builtins -Z build-std-features=compiler-builtins-mem

.PHONY: ECHO
ECHO:
	@echo "$@"

all: 
	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
	$(MAKE) -C src all ARCH=$(ARCH) || (sh -c "echo 内核编译失败" && exit 1)
	

clean:
	rm -f Cargo.lock
	$(MAKE) -C src clean ARCH=$(ARCH)

.PHONY: fmt
fmt:
	cargo fmt --all $(FMT_CHECK)

.PHONY: check
check: ECHO
# @echo "Checking kernel... ARCH=$(ARCH)"
# @exit 1
ifeq ($(ARCH), x86_64)
	@cargo +nightly-2023-08-15 check --workspace $(CARGO_ZBUILD) --message-format=json --target ./src/$(TARGET_JSON)
else ifeq ($(ARCH), riscv64)
	@cargo +nightly-2023-08-15 check --workspace $(CARGO_ZBUILD) --message-format=json --target ./src/$(TARGET_JSON)
endif

test:
# 测试内核库
	@cargo +nightly-2023-08-15 test --workspace --exclude dragonos_kernel

