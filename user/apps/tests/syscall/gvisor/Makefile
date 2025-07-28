# gvisor系统调用测试 Makefile
# 用于DragonOS项目

.PHONY: all download test clean help setup list

# 默认目标
all: setup test

# 显示帮助信息
help:
	@echo "gvisor系统调用测试 Makefile"
	@echo ""
	@echo "可用目标:"
	@echo "  download    - 下载gvisor测试套件"
	@echo "  setup       - 设置测试环境（包含下载）"
	@echo "  test        - 运行白名单中的测试（默认）"
	@echo "  test-all    - 运行所有测试（包括非白名单）"
	@echo "  test-quick  - 运行快速测试"
	@echo "  test-verbose - 详细模式运行测试"
	@echo "  list        - 列出所有可用测试"
	@echo "  clean       - 清理测试文件和结果"
	@echo "  help        - 显示此帮助信息"
	@echo ""
	@echo "环境变量:"
	@echo "  SYSCALL_TEST_WORKDIR - 测试工作目录（默认: /tmp/gvisor_tests）"
	@echo "  TEST_TIMEOUT         - 单个测试超时时间（默认: 300秒）"

# 下载测试套件
download:
	@echo "下载gvisor测试套件..."
	@./download_tests.sh

# 设置测试环境
setup: download
	@echo "设置测试环境..."
	@chmod +x run_tests.sh download_tests.sh
	@mkdir -p results

# 运行白名单中的测试（默认行为）
test: setup
	@echo "运行gvisor系统调用测试（白名单模式）..."
	@./run_tests.sh

# 运行所有测试（包括非白名单）
test-all: setup
	@echo "运行所有gvisor系统调用测试..."
	@./run_tests.sh --no-whitelist

# 运行快速测试
test-quick: setup
	@echo "运行快速测试..."
	@./run_tests.sh --timeout 60

# 详细模式运行测试
test-verbose: setup
	@echo "详细模式运行测试..."
	@./run_tests.sh -v

# 列出所有可用测试
list: setup
	@./run_tests.sh -l

# 清理测试文件和结果
clean:
	@echo "清理测试文件..."
	@rm -rf tests/
	@rm -rf results/
	@rm -f gvisor-syscalls-tests.tar.xz
	@echo "清理完成"

# 清理所有（包括下载的文件）
distclean: clean
	@echo "完全清理..."
	@rm -rf tests/ results/
	@echo "完全清理完成"
