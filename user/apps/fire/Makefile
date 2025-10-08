# Fire 容器运行时 - DragonOS 适配版本
.PHONY: all build install clean deps check help setup-source

# 变量定义
SCRIPT_DIR = $(shell pwd)
FIRE_SOURCE_DIR = $(SCRIPT_DIR)/fire-source
BUILD_ADAPTER = $(SCRIPT_DIR)/build-adapter.sh
INSTALL_DIR = $(ROOT_PATH)/bin/sysroot

# 默认目标
all: setup-source build

# 设置源码（通过 DADK 自动处理，或手动克隆）
setup-source:
	@if [ ! -d "$(FIRE_SOURCE_DIR)" ]; then \
		echo "Fire 源码目录不存在，请通过以下方式之一获取源码:"; \
		echo "1. 通过 DADK 自动克隆（推荐）"; \
		echo "2. 手动克隆: git clone <fire-repo-url> $(FIRE_SOURCE_DIR)"; \
		echo "3. 创建符号链接: ln -s /path/to/fire $(FIRE_SOURCE_DIR)"; \
		exit 1; \
	fi

# 帮助信息
help:
	@echo "Fire 容器运行时构建系统 - DragonOS 版本"
	@echo ""
	@echo "可用目标："
	@echo "  setup-source - 设置源码目录"
	@echo "  build        - 构建项目（发布模式）"
	@echo "  install      - 安装到 DragonOS sysroot"
	@echo "  clean        - 清理构建产物"
	@echo "  check        - 检查代码"
	@echo "  deps         - 检查依赖"
	@echo ""
	@echo "变量："
	@echo "  FIRE_SOURCE_DIR=$(FIRE_SOURCE_DIR)"
	@echo "  INSTALL_DIR=$(INSTALL_DIR)"
	@echo ""
	@echo "源码获取方式："
	@echo "  1. DADK 自动克隆（在 fire-0.1.0.toml 中配置）"
	@echo "  2. 手动克隆: git clone <repo> $(FIRE_SOURCE_DIR)"
	@echo "  3. 符号链接: ln -s /path/to/fire $(FIRE_SOURCE_DIR)"

# 检查依赖
deps:
	@echo "检查构建依赖..."
	@which rustc > /dev/null || (echo "错误: 未找到 Rust 编译器" && exit 1)
	@which cargo > /dev/null || (echo "错误: 未找到 Cargo" && exit 1)
	@which x86_64-linux-musl-gcc > /dev/null || (echo "警告: 未找到 musl 交叉编译器，可能需要安装 musl-tools")
	@echo "依赖检查完成"

# 构建项目（发布模式）
build: setup-source deps
	@echo "通过构建适配脚本构建 Fire..."
	chmod +x $(BUILD_ADAPTER)
	$(BUILD_ADAPTER) build

# 代码检查
check: setup-source deps
	@echo "通过构建适配脚本检查代码..."
	chmod +x $(BUILD_ADAPTER)
	$(BUILD_ADAPTER) check

# 安装到 DragonOS sysroot
install: setup-source deps
	@echo "通过构建适配脚本安装 Fire..."
	chmod +x $(BUILD_ADAPTER)
	$(BUILD_ADAPTER) install

# 清理构建产物
clean:
	@echo "清理 Fire 构建产物..."
	if [ -d "$(FIRE_SOURCE_DIR)" ]; then \
		chmod +x $(BUILD_ADAPTER); \
		$(BUILD_ADAPTER) clean; \
	fi

# 显示构建信息
info:
	@echo "Fire 容器运行时构建信息:"
	@echo "  源码目录: $(FIRE_SOURCE_DIR)"
	@echo "  构建适配脚本: $(BUILD_ADAPTER)"
	@echo "  安装目录: $(INSTALL_DIR)"
	@echo "  ROOT_PATH: $(ROOT_PATH)"
	@if command -v cargo >/dev/null 2>&1; then \
		echo "  Cargo 版本: $(shell cargo --version)"; \
	fi
	@if command -v rustc >/dev/null 2>&1; then \
		echo "  Rust 版本: $(shell rustc --version)"; \
	fi

# 克隆源码（手动方式）
clone-source:
	@echo "请手动克隆 Fire 源码:"
	@echo "git clone <fire-repository-url> $(FIRE_SOURCE_DIR)"
	@echo ""
	@echo "或者创建符号链接:"
	@echo "ln -s /path/to/existing/fire $(FIRE_SOURCE_DIR)"

# 链接现有源码
link-source:
	@if [ -z "$(FIRE_PATH)" ]; then \
		echo "错误: 请设置 FIRE_PATH 变量"; \
		echo "使用方法: make link-source FIRE_PATH=/path/to/fire"; \
		exit 1; \
	fi
	@if [ ! -d "$(FIRE_PATH)" ]; then \
		echo "错误: 指定的路径不存在: $(FIRE_PATH)"; \
		exit 1; \
	fi
	ln -sf "$(FIRE_PATH)" "$(FIRE_SOURCE_DIR)"
	@echo "已创建符号链接: $(FIRE_SOURCE_DIR) -> $(FIRE_PATH)"