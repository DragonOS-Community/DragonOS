user_sub_dirs = apps

ROOT_PATH ?= $(abspath ..)
DADK ?= dadk

DADK_BIN = $(or $(strip $(DADK)),dadk)
DADK_CACHE_DIR = $(ROOT_PATH)/bin/dadk_cache
# 统一使用由 tools/rootfs_manifest_resolve.sh 生成的 manifest
DADK_MANIFEST_FILE ?= $(ROOT_PATH)/dadk-manifest.generated.toml
SYSROOT_INSTALL_MARKER = $(ROOT_PATH)/bin/sysroot/.dadk_install_marker
# Sysroot layout version. Layout 1 is the legacy layout where sysconfig was
# copied into bin/sysroot via cp -r, leaking host metadata into the image.
# Bumping this version triggers a one-time removal and DADK reinstall of
# bin/sysroot (keeping bin/dadk_cache) to remove files from previous layouts.
# Layout 3 removes the legacy C test binaries.
SYSROOT_LAYOUT_VERSION = 3
ROOTFS_MANIFEST ?= default

ECHO:
	@echo "$@"


.PHONY: install_dadk
install_dadk:
	@bash "$(ROOT_PATH)/tools/install-dadk.sh" "$(DADK_BIN)"

.PHONY: dadk_run
dadk_run: install_dadk
	@test -f "$(DADK_MANIFEST_FILE)" || (echo "Error: missing $(DADK_MANIFEST_FILE), run 'make prepare_rootfs_manifest' at repo root first." && exit 1)
	mkdir -p $(DADK_CACHE_DIR)
	@marker_expected="manifest=$(ROOTFS_MANIFEST);layout=$(SYSROOT_LAYOUT_VERSION)"; \
	if [ -f "$(ROOT_PATH)/config/rootfs.generated.toml" ]; then \
		rootfs_hash="$$(sha256sum "$(ROOT_PATH)/config/rootfs.generated.toml" | awk '{print $$1}')"; \
		marker_expected="$$marker_expected;rootfs=$$rootfs_hash"; \
	fi; \
	if [ -f "$(DADK_MANIFEST_FILE)" ]; then \
		dadk_hash="$$(sha256sum "$(DADK_MANIFEST_FILE)" | awk '{print $$1}')"; \
		marker_expected="$$marker_expected;dadk=$$dadk_hash"; \
	fi; \
	marker_current="$$(cat "$(SYSROOT_INSTALL_MARKER)" 2>/dev/null || true)"; \
	marker_changed=0; \
	if [ -f "$(SYSROOT_INSTALL_MARKER)" ] && [ "$$marker_current" != "$$marker_expected" ]; then \
		marker_changed=1; \
	fi; \
	if [ "$$marker_changed" = "1" ] || [ ! -f "$(SYSROOT_INSTALL_MARKER)" ] || [ ! -d "$(ROOT_PATH)/bin/sysroot/bin" ] || [ -z "$$(ls -A "$(ROOT_PATH)/bin/sysroot/bin" 2>/dev/null)" ]; then \
		echo "sysroot install marker/status invalid (marker=$$marker_current, expected=$$marker_expected), force reinstall by cleaning DADK output cache..."; \
		if [ "$$marker_changed" = "1" ]; then \
			echo "manifest changed, removing $(ROOT_PATH)/bin/sysroot before reinstall..."; \
			rm -rf "$(ROOT_PATH)/bin/sysroot"; \
			mkdir -p "$(ROOT_PATH)/bin/sysroot"; \
		fi; \
		"$(DADK_BIN)" -f $(DADK_MANIFEST_FILE) user clean --level output -w $(ROOT_PATH) || true; \
	fi
	"$(DADK_BIN)" -f $(DADK_MANIFEST_FILE) user build -w $(ROOT_PATH)
	"$(DADK_BIN)" -f $(DADK_MANIFEST_FILE) user install -w $(ROOT_PATH)
	@marker_expected="manifest=$(ROOTFS_MANIFEST);layout=$(SYSROOT_LAYOUT_VERSION)"; \
	if [ -f "$(ROOT_PATH)/config/rootfs.generated.toml" ]; then \
		rootfs_hash="$$(sha256sum "$(ROOT_PATH)/config/rootfs.generated.toml" | awk '{print $$1}')"; \
		marker_expected="$$marker_expected;rootfs=$$rootfs_hash"; \
	fi; \
	if [ -f "$(DADK_MANIFEST_FILE)" ]; then \
		dadk_hash="$$(sha256sum "$(DADK_MANIFEST_FILE)" | awk '{print $$1}')"; \
		marker_expected="$$marker_expected;dadk=$$dadk_hash"; \
	fi; \
	echo "$$marker_expected" > $(SYSROOT_INSTALL_MARKER)

.PHONY: dadk_clean
dadk_clean: install_dadk
	@echo dadk_clean

all:
	mkdir -p $(ROOT_PATH)/bin/sysroot

	$(MAKE) dadk_run
	$(MAKE) prepare_sysconfig_payload

	@echo 用户态程序编译完成

# Build the deterministic sysconfig payload (bin/sysconfig.tar).
# sysconfig no longer goes into bin/sysroot; tools/write_disk_image.sh
# imports it last when writing the disk image. The output is insensitive
# to the host UID/GID/umask.
.PHONY: prepare_sysconfig_payload
prepare_sysconfig_payload:
	python3 $(ROOT_PATH)/tools/build_sysconfig_payload.py


.PHONY: clean
clean:
	$(MAKE) dadk_clean
	rm -f $(ROOT_PATH)/bin/sysconfig.tar
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

.PHONY: fmt
fmt:
	FMT_CHECK=$(FMT_CHECK) $(MAKE) -C apps fmt
