ifdef DADK_CURRENT_BUILD_DIR
INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
INSTALL_DIR = ./install
endif

TOOLCHAIN = +nightly-2026-02-24-x86_64-unknown-linux-gnu
RUSTFLAGS +=
ifeq ($(ARCH), x86_64)
	export RUST_TARGET = x86_64-unknown-linux-musl
else ifeq ($(ARCH), riscv64)
	export RUST_TARGET = riscv64gc-unknown-linux-gnu
else
	export RUST_TARGET = x86_64-unknown-linux-musl
endif

RUNNER_DIR = runner
RUNNER_BIN = $(RUNNER_DIR)/target/$(RUST_TARGET)/release/dunitest-runner
RESULTS_DIR = results
BIN_DIR = bin

# 要编译的测试套件目录列表（suites/ 下的子目录名）
SUITES = demo normal fuse

GTEST_ROOT = third_party/googletest
GTEST_REPO = https://git.mirrors.dragonos.org.cn/DragonOS-Community/googletest
GTEST_TAG = v1.17.0
GTEST_SRC = $(GTEST_ROOT)/googletest/src/gtest-all.cc
GTEST_TMP_ROOT = $(GTEST_ROOT).tmp
CXX ?= g++
CXXFLAGS ?= -Wall -O2 -std=c++17 -pthread
GTEST_INCLUDES = -I$(GTEST_ROOT)/googletest -I$(GTEST_ROOT)/googletest/include
SUITE_SRCS = $(foreach suite,$(SUITES),$(wildcard suites/$(suite)/*.cc))
TEST_BINS = $(patsubst suites/%.cc,$(BIN_DIR)/%_test,$(SUITE_SRCS))

.PHONY: all build build-suites run list test-local install clean fmt fetch-gtest

all: build install

build: build-suites
	@echo "构建 dunitest runner..."
	@cd $(RUNNER_DIR) && RUSTFLAGS=$(RUSTFLAGS) cargo $(TOOLCHAIN) build --target $(RUST_TARGET) --release

fetch-gtest: $(GTEST_SRC)

$(GTEST_SRC):
	@echo "拉取 googletest ($(GTEST_TAG))..."
	@rm -rf "$(GTEST_TMP_ROOT)"
	@mkdir -p "$(dir $(GTEST_ROOT))"
	@if [ -d "$(GTEST_ROOT)" ]; then \
		echo "检测到残缺的 $(GTEST_ROOT)，重新拉取..."; \
		rm -rf "$(GTEST_ROOT)"; \
	fi
	@git clone --depth 1 --branch "$(GTEST_TAG)" "$(GTEST_REPO)" "$(GTEST_TMP_ROOT)"
	@mv "$(GTEST_TMP_ROOT)" "$(GTEST_ROOT)"
	@test -f "$(GTEST_SRC)"

build-suites: $(TEST_BINS)

$(BIN_DIR)/%_test: suites/%.cc $(GTEST_SRC)
	@mkdir -p "$(dir $@)"
	@echo "编译测例: $< -> $@"
	@$(CXX) $(CXXFLAGS) $(GTEST_INCLUDES) "$<" $(GTEST_SRC) -o "$@"

run: build
	@$(RUNNER_BIN) --bin-dir $(BIN_DIR) --results-dir $(RESULTS_DIR)

list: build
	@$(RUNNER_BIN) --bin-dir $(BIN_DIR) --list

test-local: build
	@$(RUNNER_BIN) --bin-dir $(BIN_DIR) --results-dir $(RESULTS_DIR) --verbose

install: build
	@echo "安装 dunitest 到 $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@cp -f $(RUNNER_BIN) $(INSTALL_DIR)/dunitest-runner
	@chmod +x $(INSTALL_DIR)/dunitest-runner
	@cp -rf $(BIN_DIR) $(INSTALL_DIR)/
	@cp -f whitelist.txt $(INSTALL_DIR)/whitelist.txt
	@cp -f scripts/run_tests.sh $(INSTALL_DIR)/run_tests.sh
	@chmod +x $(INSTALL_DIR)/run_tests.sh
	@echo "安装完成"

clean:
	@rm -rf $(RESULTS_DIR) install $(BIN_DIR)
	@cd $(RUNNER_DIR) && cargo clean

fmt:
	@cd $(RUNNER_DIR) && cargo fmt
