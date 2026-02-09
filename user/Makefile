user_sub_dirs = apps

ROOT_PATH ?= $(abspath ..)
DADK ?= dadk

DADK_VERSION=$(shell dadk -V | awk 'END {print $$2}')
# 最小的DADK版本
MIN_DADK_VERSION = 0.6.0
DADK_CACHE_DIR = $(ROOT_PATH)/bin/dadk_cache
# 统一使用由 tools/rootfs_manifest_resolve.sh 生成的 manifest
DADK_MANIFEST_FILE ?= $(ROOT_PATH)/dadk-manifest.generated.toml
SYSROOT_INSTALL_MARKER = $(ROOT_PATH)/bin/sysroot/.dadk_install_marker
ROOTFS_MANIFEST ?= default

ECHO:
	@echo "$@"


install_dadk:
# 如果未安装
ifeq ("$(DADK_VERSION)", "")
	@echo "\ndadk is not installed."
	@echo "Please install dadk $(MIN_DADK_VERSION) or higher version"
	@echo "\nYou can install dadk by running the following command:"
	@echo "\n\tcargo install dadk"
	@echo "\nOr you can install dadk from source by running the following command:"
	@echo "\n\tcargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v$(MIN_DADK_VERSION) --locked"
	@echo "\n"
	@echo "Auto installing dadk..."
	cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v$(MIN_DADK_VERSION) --locked
else
# 如果DADK版本过低，则自动更新
	@echo "dadk version $(DADK_VERSION) installed"
# 如果DADK版本过低，则自动更新
ifneq ($(shell printf '%s\n%s' "$(DADK_VERSION)" "$(MIN_DADK_VERSION)" | sort -V | head -n1), $(MIN_DADK_VERSION))
	@echo "dadk version is too low, please update to $(MIN_DADK_VERSION) or higher version"
	@echo "Updating dadk..."
	cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v$(MIN_DADK_VERSION) --locked || (echo "dadk update failed" && exit 1)
	@echo "dadk updated"
endif
endif

.PHONY: dadk_run
dadk_run: install_dadk
	@test -f "$(DADK_MANIFEST_FILE)" || (echo "Error: missing $(DADK_MANIFEST_FILE), run 'make prepare_rootfs_manifest' at repo root first." && exit 1)
	mkdir -p $(DADK_CACHE_DIR)
	@marker_expected="manifest=$(ROOTFS_MANIFEST)"; \
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
		$(DADK) -f $(DADK_MANIFEST_FILE) user clean --level output -w $(ROOT_PATH) || true; \
	fi
	$(DADK) -f $(DADK_MANIFEST_FILE) user build -w $(ROOT_PATH)
	$(DADK) -f $(DADK_MANIFEST_FILE) user install -w $(ROOT_PATH)
	@marker_expected="manifest=$(ROOTFS_MANIFEST)"; \
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
	$(MAKE) copy_sysconfig

	@echo 用户态程序编译完成

.PHONY: copy_sysconfig
copy_sysconfig:
	cp -r sysconfig/* $(ROOT_PATH)/bin/sysroot/


.PHONY: clean
clean:
	$(MAKE) dadk_clean
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

.PHONY: fmt
fmt:
	FMT_CHECK=$(FMT_CHECK) $(MAKE) -C apps fmt
