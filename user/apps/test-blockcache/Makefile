# The toolchain we use.
# You can get it by running DragonOS' `tools/bootstrap.sh`
TOOLCHAIN="+nightly-2023-08-15-x86_64-unknown-linux_dragonos-gnu"
RUSTFLAGS+="-C target-feature=+crt-static -C link-arg=-no-pie"

# 如果是在dadk中编译，那么安装到dadk的安装目录中
INSTALL_DIR?=$(DADK_CURRENT_BUILD_DIR)
# 如果是在本地编译，那么安装到当前目录下的install目录中
INSTALL_DIR?=./install


run:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) run

build:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) build

clean:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) clean

test:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) test

doc:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) doc

run-release:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) run --release

build-release:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) build --release

clean-release:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) clean --release

test-release:
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) test --release

.PHONY: install
install: 
	RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) install --path . --no-track --root $(INSTALL_DIR) --force
