# LMbench benchmark test suite Makefile.
# Shell-only: no compilation, just install the guest-side files into the rootfs.
# Invoked by DADK (build-from-source) with DADK_CURRENT_BUILD_DIR set.

ifdef DADK_CURRENT_BUILD_DIR
INSTALL_DIR = $(DADK_CURRENT_BUILD_DIR)
else
INSTALL_DIR = ./install
endif

# Files needed inside the guest at runtime. Sources live under runner/;
# `install` flattens them into INSTALL_DIR (guest runtime layout is flat).
RUNNER_DIR    = runner
GUEST_SCRIPTS = $(RUNNER_DIR)/run.sh $(RUNNER_DIR)/init.sh $(RUNNER_DIR)/env.sh $(RUNNER_DIR)/clean_up.sh
GUEST_DATA    = whitelist.txt config

.PHONY: all build install clean help

all: install

# No build step (shell scripts only); kept so DADK's default flow is happy.
build:
	@true

install:
	@echo "[lmbench] installing to $(INSTALL_DIR)"
	@mkdir -p $(INSTALL_DIR)
	@rm -f $(INSTALL_DIR)/run_tests.sh $(INSTALL_DIR)/run_test_case.sh
	@install -m755 $(GUEST_SCRIPTS) $(INSTALL_DIR)/
	@install -m644 $(GUEST_DATA) $(INSTALL_DIR)/
	@cp -r $(RUNNER_DIR)/test_cases $(INSTALL_DIR)/
	@chmod +x $(INSTALL_DIR)/test_cases/*.sh 2>/dev/null || true
	@echo "[lmbench] install done"

clean:
	@rm -rf ./install

help:
	@echo "lmbench benchmark Makefile targets:"
	@echo "  install  - copy runner + test cases into INSTALL_DIR (DADK build dir)"
	@echo "  clean    - remove local ./install"
