ARCH ?= x86_64

ifeq ($(ARCH), x86_64)
	CROSS_COMPILE = x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE = riscv64-linux-musl-
endif

ifeq ($(origin CXX), default)
	CXX = $(CROSS_COMPILE)g++
endif
CXXFLAGS ?= -Wall -O2 -std=c++17 -pthread -static

BIN = virtiofs_bench
SRC = virtiofs_bench.cc

.PHONY: all install clean fmt test-host

all: $(BIN)

$(BIN): $(SRC)
	$(CXX) $(CXXFLAGS) $< -o $@

install: all
	@echo "Installing $(BIN) to $(DADK_CURRENT_BUILD_DIR)/"
	@mkdir -p "$(DADK_CURRENT_BUILD_DIR)"
	cp -f "$(BIN)" "$(DADK_CURRENT_BUILD_DIR)/$(BIN)"

clean:
	rm -f "$(BIN)"

fmt:

test-host:
	sh ./transcript_test.sh
