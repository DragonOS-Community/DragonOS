ARCH ?= x86_64
CROSS_COMPILE ?= $(ARCH)-linux-musl-
ifeq ($(origin CC), default)
CC = $(CROSS_COMPILE)gcc
endif
CFLAGS ?= -O2 -std=c11 -Wall -Wextra -Werror -static
BIN = agent_memory_guest
.PHONY: all install clean fmt test-host
all: $(BIN)
$(BIN): agent_memory_guest.c
	$(CC) $(CFLAGS) $< -o $@
install: all
	mkdir -p "$(DADK_CURRENT_BUILD_DIR)"
	cp $(BIN) "$(DADK_CURRENT_BUILD_DIR)/$(BIN)"
clean:
	rm -f $(BIN)
fmt:
	clang-format -i agent_memory_guest.c
test-host:
	python3 test_helper.py
