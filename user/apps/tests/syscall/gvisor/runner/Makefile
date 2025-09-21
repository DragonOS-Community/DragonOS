# Makefile for gvisor-test-runner

# 默认目标
.PHONY: all build release clean install test help

# 默认构建
all: build

# 开发构建
build:
	cargo build

# Release构建（推荐）
release:
	cargo build --release

# 清理构建文件
clean:
	cargo clean

# 运行测试（如果有单元测试）
test:
	cargo test

# 检查代码
check:
	cargo check

# 格式化代码
fmt:
	cargo fmt

# 代码检查
clippy:
	cargo clippy

# 列出测试用例
list: release
	./target/release/runner --list

# 运行测试（使用默认配置）
run: release
	./target/release/runner

# 显示帮助
help: release
	./target/release/runner --help

# 显示此Makefile的帮助
show-help:
	@echo "可用的make目标："
	@echo "  all      - 构建开发版本（默认）"
	@echo "  build    - 构建开发版本"
	@echo "  release  - 构建release版本（推荐）"
	@echo "  clean    - 清理构建文件"
	@echo "  install  - 安装到系统（需要sudo）"
	@echo "  test     - 运行单元测试"
	@echo "  check    - 检查代码编译"
	@echo "  fmt      - 格式化代码"
	@echo "  clippy   - 运行代码检查"
	@echo "  list     - 列出所有测试用例"
	@echo "  run      - 运行测试（默认配置）"
	@echo "  help     - 显示程序帮助"
	@echo ""
	@echo "使用示例："
	@echo "  make release                    # 构建release版本"
	@echo "  make list                       # 列出测试用例"  
	@echo "  make run                        # 运行白名单中的测试"
	@echo "  ./target/release/runner --help  # 查看详细帮助"
