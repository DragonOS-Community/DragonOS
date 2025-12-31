# Nix environment doesn't support rustup-style toolchain selection
# TOOLCHAIN is exported from kernel/Makefile, fallback to default if not set
ifeq ($(USING_DRAGONOS_NIX_ENV),1)
TOOLCHAIN ?=
else
TOOLCHAIN ?= +nightly-2025-08-10
endif

all:
	@cargo $(TOOLCHAIN) build --release -p gen_kallsyms
fmt:
	cargo fmt --all $(FMT_CHECK)
clean:
	@cargo clean
check:
	@cargo $(TOOLCHAIN) check --workspace $(CARGO_ZBUILD) --message-format=json
.PHONY: fmt
