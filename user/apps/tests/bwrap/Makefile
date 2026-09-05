# bubblewrap功能测试 Makefile
# 用于DragonOS项目

ifdef DADK_CURRENT_BUILD_DIR
INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
INSTALL_DIR = ./install
endif

.PHONY: all install install-bwrap download test clean help

all: install

help:
	@echo "bubblewrap功能测试 Makefile"
	@echo ""
	@echo "可用目标:"
	@echo "  all           - 安装测试文件（默认）"
	@echo "  install       - 拷贝测试脚本和配置到安装目录"
	@echo "  install-bwrap - 仅拷贝bwrap二进制到安装目录（给DADK /bin用）"
	@echo "  download      - 下载bwrap测试套件"
	@echo "  test          - 在本地运行白名单中的测试"
	@echo "  clean         - 清理安装目录"
	@echo "  help          - 显示此帮助信息"

download:
	@./download_tests.sh --skip-if-exists

install: download whitelist.txt
	@echo "安装bwrap测试套件到 $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@cp -f whitelist.txt $(INSTALL_DIR)/
	@cp -rf tests $(INSTALL_DIR)/
	@cp -f run_tests.sh $(INSTALL_DIR)/
	@chmod +x $(INSTALL_DIR)/run_tests.sh
	@echo "安装完成"

install-bwrap: download
	@mkdir -p $(INSTALL_DIR)
	@cp -f tests/bwrap-static $(INSTALL_DIR)/bwrap
	@chmod +x $(INSTALL_DIR)/bwrap

test:
	@BWRAP=$${BWRAP:-bwrap} /bin/sh run_tests.sh

clean:
	@rm -rf $(INSTALL_DIR)
	@echo "清理完成"
