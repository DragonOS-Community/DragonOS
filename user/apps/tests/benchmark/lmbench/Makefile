# LMbench benchmark test suite Makefile.
# Shell-only: no compilation, just install the guest-side files into the rootfs.
# Invoked by DADK (build-from-source) with DADK_CURRENT_BUILD_DIR set.

ifdef DADK_CURRENT_BUILD_DIR
INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
INSTALL_DIR = ./install
endif

# Files needed inside the guest at runtime.
GUEST_SCRIPTS = run_tests.sh init.sh env.sh clean_up.sh run_test_case.sh
GUEST_DATA    = whitelist.txt config

.PHONY: all build install clean help

all: install

# No build step (shell scripts only); kept so DADK's default flow is happy.
build:
	@true

install:
	@echo "[lmbench] installing to $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@install -m755 $(GUEST_SCRIPTS) $(INSTALL_DIR)/
	@install -m644 $(GUEST_DATA) $(INSTALL_DIR)/
	@cp -r test_cases $(INSTALL_DIR)/
	@chmod +x $(INSTALL_DIR)/test_cases/*.sh 2>/dev/null || true
	@echo "[lmbench] install done"

clean:
	@rm -rf ./install

help:
	@echo "lmbench benchmark Makefile targets:"
	@echo "  install  - copy runner + test cases into INSTALL_DIR (DADK build dir)"
	@echo "  clean    - remove local ./install"
