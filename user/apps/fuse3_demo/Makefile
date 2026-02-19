ARCH ?= x86_64
ifeq ($(ARCH), x86_64)
	CROSS_COMPILE = x86_64-linux-musl-
else ifeq ($(ARCH), riscv64)
	CROSS_COMPILE = riscv64-linux-musl-
endif

CC = $(CROSS_COMPILE)gcc
AR = $(CROSS_COMPILE)ar
RANLIB = $(CROSS_COMPILE)ranlib
STRIP = $(CROSS_COMPILE)strip

PKG_CONFIG ?= pkg-config
MESON ?= meson

LIBFUSE_VERSION ?= 3.18.1
LIBFUSE_NAME := fuse-$(LIBFUSE_VERSION)
LIBFUSE_URL_PRIMARY ?= https://git.mirrors.dragonos.org.cn/DragonOS-Community/libfuse/archive/fuse-$(LIBFUSE_VERSION).tar.gz
LIBFUSE_URL_MIRROR ?= https://github.com/libfuse/libfuse/releases/download/fuse-$(LIBFUSE_VERSION)/$(LIBFUSE_NAME).tar.gz

LIBFUSE_CACHE_DIR := $(CURDIR)/.cache
LIBFUSE_ARCHIVE ?= $(LIBFUSE_CACHE_DIR)/$(LIBFUSE_NAME).tar.gz
LIBFUSE_SRC_ROOT := $(CURDIR)/.build/libfuse-src
LIBFUSE_SRC_DIR := $(LIBFUSE_SRC_ROOT)/$(LIBFUSE_NAME)
LIBFUSE_SRC_ALT_DIR := $(LIBFUSE_SRC_ROOT)/libfuse
LIBFUSE_BUILD_DIR := $(CURDIR)/.build/libfuse-build-$(ARCH)
LIBFUSE_PREFIX := $(CURDIR)/.build/libfuse-install-$(ARCH)
LIBFUSE_PC_DIR := $(LIBFUSE_PREFIX)/lib/pkgconfig
LIBFUSE_STAMP := $(LIBFUSE_PREFIX)/.built-$(LIBFUSE_VERSION)

CFLAGS_COMMON := -Wall -Wextra -O2 -static -D_FILE_OFFSET_BITS=64
LIBFUSE_MESON_CFLAGS ?= -static
LIBFUSE_MESON_LDFLAGS ?= -static
LIBFUSE_MESON_JOBS ?= 1

BINS := fuse3_demo test_fuse3_demo

all: $(BINS)
	@echo "bins: $(BINS)"

$(LIBFUSE_ARCHIVE):
	@mkdir -p $(LIBFUSE_CACHE_DIR)
	@if [ -s "$@" ]; then \
		echo "[libfuse] use cached archive: $@"; \
		exit 0; \
	fi
	@echo "[libfuse] downloading $(LIBFUSE_VERSION)"
	@if command -v curl >/dev/null 2>&1; then \
		curl -fL --retry 3 --retry-delay 1 -o "$@.tmp" "$(LIBFUSE_URL_PRIMARY)" || \
		curl -fL --retry 3 --retry-delay 1 -o "$@.tmp" "$(LIBFUSE_URL_MIRROR)"; \
	elif command -v wget >/dev/null 2>&1; then \
		wget -O "$@.tmp" "$(LIBFUSE_URL_PRIMARY)" || \
		wget -O "$@.tmp" "$(LIBFUSE_URL_MIRROR)"; \
	else \
		echo "error: neither curl nor wget found"; \
		exit 1; \
	fi
	@mv "$@.tmp" "$@"

$(LIBFUSE_SRC_DIR): $(LIBFUSE_ARCHIVE)
	@mkdir -p $(LIBFUSE_SRC_ROOT)
	@rm -rf "$(LIBFUSE_SRC_DIR)" "$(LIBFUSE_SRC_ALT_DIR)"
	@tar -xzf "$(LIBFUSE_ARCHIVE)" -C "$(LIBFUSE_SRC_ROOT)"
	@if [ ! -f "$(LIBFUSE_SRC_DIR)/meson.build" ] && [ -f "$(LIBFUSE_SRC_ALT_DIR)/meson.build" ]; then \
		mv "$(LIBFUSE_SRC_ALT_DIR)" "$(LIBFUSE_SRC_DIR)"; \
	fi
	@test -f "$(LIBFUSE_SRC_DIR)/meson.build" || ( \
		echo "error: libfuse source tree missing meson.build in $(LIBFUSE_SRC_DIR)"; \
		exit 1 \
	)

$(LIBFUSE_STAMP): $(LIBFUSE_ARCHIVE)
	@command -v "$(MESON)" >/dev/null 2>&1 || (echo "error: meson not found" && exit 1)
	@command -v ninja >/dev/null 2>&1 || (echo "error: ninja not found" && exit 1)
	@command -v "$(PKG_CONFIG)" >/dev/null 2>&1 || (echo "error: pkg-config not found" && exit 1)
	@if [ ! -f "$(LIBFUSE_SRC_DIR)/meson.build" ]; then \
		echo "[libfuse] source tree missing or stale, re-extracting..."; \
		mkdir -p "$(LIBFUSE_SRC_ROOT)"; \
		rm -rf "$(LIBFUSE_SRC_DIR)" "$(LIBFUSE_SRC_ALT_DIR)"; \
		tar -xzf "$(LIBFUSE_ARCHIVE)" -C "$(LIBFUSE_SRC_ROOT)"; \
		if [ ! -f "$(LIBFUSE_SRC_DIR)/meson.build" ] && [ -f "$(LIBFUSE_SRC_ALT_DIR)/meson.build" ]; then \
			mv "$(LIBFUSE_SRC_ALT_DIR)" "$(LIBFUSE_SRC_DIR)"; \
		fi; \
	fi
	@test -f "$(LIBFUSE_SRC_DIR)/meson.build" || ( \
		echo "error: libfuse source tree missing meson.build in $(LIBFUSE_SRC_DIR)"; \
		exit 1 \
	)
	@rm -rf "$(LIBFUSE_BUILD_DIR)" "$(LIBFUSE_PREFIX)"
	@mkdir -p "$(LIBFUSE_BUILD_DIR)" "$(LIBFUSE_PREFIX)"
	@CC="$(CC)" AR="$(AR)" RANLIB="$(RANLIB)" STRIP="$(STRIP)" \
		CFLAGS="$(LIBFUSE_MESON_CFLAGS)" LDFLAGS="$(LIBFUSE_MESON_LDFLAGS)" \
		$(MESON) setup "$(LIBFUSE_BUILD_DIR)" "$(LIBFUSE_SRC_DIR)" \
		--buildtype=release \
		--default-library=static \
		--prefix="$(LIBFUSE_PREFIX)" \
		--libdir=lib \
		-Dexamples=false \
		-Dutils=false \
		-Ddisable-mtab=true \
		-Duseroot=false
	@env -u MAKEFLAGS $(MESON) compile -C "$(LIBFUSE_BUILD_DIR)" -j "$(LIBFUSE_MESON_JOBS)"
	@env -u MAKEFLAGS $(MESON) install -C "$(LIBFUSE_BUILD_DIR)"
	@touch "$@"

fuse3_demo: fuse3_demo.c $(LIBFUSE_STAMP)
	@FUSE_CFLAGS="$$(PKG_CONFIG_PATH="$(LIBFUSE_PC_DIR)" $(PKG_CONFIG) --cflags fuse3)"; \
	FUSE_LIBS="$$(PKG_CONFIG_PATH="$(LIBFUSE_PC_DIR)" $(PKG_CONFIG) --static --libs fuse3)"; \
	$(CC) $(CFLAGS_COMMON) $$FUSE_CFLAGS $< -o $@ $$FUSE_LIBS

test_fuse3_demo: test_fuse3_demo.c
	$(CC) $(CFLAGS_COMMON) $< -o $@

install: all
	@echo "Installing binaries to $(DADK_CURRENT_BUILD_DIR)/"
	mv $(BINS) $(DADK_CURRENT_BUILD_DIR)/

clean:
	rm -f $(BINS)
	rm -rf "$(LIBFUSE_BUILD_DIR)" "$(LIBFUSE_PREFIX)"

distclean: clean
	rm -rf "$(LIBFUSE_SRC_ROOT)" "$(LIBFUSE_CACHE_DIR)"

.PHONY: all install clean distclean
