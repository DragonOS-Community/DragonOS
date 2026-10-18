TOOLCHAIN="+nightly-2026-02-24-x86_64-unknown-linux-gnu"

ifdef DADK_CURRENT_BUILD_DIR
	INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
	INSTALL_DIR = ./install
endif

ifeq ($(ARCH), x86_64)
	export RUST_TARGET=x86_64-unknown-linux-musl
else
	export RUST_TARGET=x86_64-unknown-linux-musl
endif

install: build
	@mkdir -p $(INSTALL_DIR)
	strip target/$(RUST_TARGET)/release/skillfs-probe -o $(INSTALL_DIR)/skillfs-probe

build:
	cargo $(TOOLCHAIN) build --target $(RUST_TARGET) --release

clean:
	cargo $(TOOLCHAIN) clean

fmt:
	cargo $(TOOLCHAIN) fmt

fmt-check:
	cargo $(TOOLCHAIN) fmt --check

.PHONY: build install clean fmt fmt-check
