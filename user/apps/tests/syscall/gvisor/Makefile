# gvisor系统调用测试 Makefile
# 用于DragonOS项目

# Rust工具链配置
TOOLCHAIN="+nightly-2025-08-10-x86_64-unknown-linux-gnu"
RUSTFLAGS+=""

ifdef DADK_CURRENT_BUILD_DIR
# 如果是在dadk中编译，那么安装到dadk的安装目录中
INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
# 如果是在本地编译，那么安装到当前目录下的install目录中
INSTALL_DIR = ./install
endif

ifeq ($(ARCH), x86_64)
	export RUST_TARGET=x86_64-unknown-linux-musl
else ifeq ($(ARCH), riscv64)
	export RUST_TARGET=riscv64gc-unknown-linux-gnu
else
# 默认为x86_64，用于本地编译
	export RUST_TARGET=x86_64-unknown-linux-musl
endif

.PHONY: all build install download test list run clean help

# 默认目标：构建并安装
all: build install

# 显示帮助信息
help:
	@echo "gvisor系统调用测试 Makefile"
	@echo ""
	@echo "可用目标:"
	@echo "  all         - 构建并安装测试运行器（默认）"
	@echo "  build       - 构建Rust测试运行器"
	@echo "  install     - 安装测试运行器和必要文件"
	@echo "  download    - 下载gvisor测试套件"
	@echo "  test        - 运行白名单中的测试"
	@echo "  list        - 列出所有可用测试"
	@echo "  run         - 运行测试并传递参数（如：make run ARGS=\"-v epoll_test\")"
	@echo "  clean       - 清理测试文件和结果"
	@echo "  help        - 显示此帮助信息"
	@echo ""
	@echo "环境变量:"
	@echo "  SYSCALL_TEST_WORKDIR - 测试工作目录（默认: /tmp/gvisor_tests）"
	@echo "  TEST_TIMEOUT         - 单个测试超时时间（默认: 300秒）"

# 构建Rust测试运行器
build:
	@echo "构建gvisor测试运行器..."
	@cd runner && RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) build --target $(RUST_TARGET) --release

# 下载测试套件
download:
	@echo "下载gvisor测试套件..."
	@./download_tests.sh --skip-if-exists

# 安装到目标目录
install: build download
	@echo "安装gvisor测试套件到 $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	# 安装Rust测试运行器二进制文件
	@cp -f runner/target/$(RUST_TARGET)/release/runner $(INSTALL_DIR)/gvisor-test-runner
	@chmod +x $(INSTALL_DIR)/gvisor-test-runner
	# 安装测试配置文件
	@cp -f whitelist.txt $(INSTALL_DIR)/
	@cp -rf blocklists $(INSTALL_DIR)/
	@cp -rf tests $(INSTALL_DIR)/
	@echo "安装完成"

# 运行测试
test: build
	@echo "运行gvisor系统调用测试..."
	@if [ ! -d tests ]; then \
		echo "测试套件不存在，正在下载..."; \
		./download_tests.sh; \
	fi
	@./runner/target/$(RUST_TARGET)/release/runner

# 列出所有测试
list: build
	@if [ ! -d tests ]; then \
		echo "测试套件不存在，正在下载..."; \
		./download_tests.sh; \
	fi
	@./runner/target/$(RUST_TARGET)/release/runner --list

# 运行测试并传递参数
run: build
	@if [ ! -d tests ]; then \
		echo "测试套件不存在，正在下载..."; \
		./download_tests.sh; \
	fi
	@./runner/target/$(RUST_TARGET)/release/runner $(ARGS)

# 清理
clean:
	@echo "清理测试文件和结果..."
	@rm -rf results/
	@rm -f gvisor-syscalls-tests.tar.xz
	@cd runner && cargo clean
	@echo "清理完成"
