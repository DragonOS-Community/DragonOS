user_sub_dirs = apps

DADK_VERSION=$(shell dadk -V | awk 'END {print $$2}')
# 最小的DADK版本
MIN_DADK_VERSION = 0.2.0
DADK_CACHE_DIR = $(ROOT_PATH)/bin/$(ARCH)/dadk_cache
SYSROOT_DIR = $(ROOT_PATH)/bin/$(ARCH)/sysroot
DADK_MANIFEST_PATH = $(ROOT_PATH)/oscomp/manifest-${ARCH}.toml

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
	@echo "\n\tcargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v$(MIN_DADK_VERSION)"
	@echo "\n"
	@echo "Auto installing dadk..."
	cargo install dadk
else
# 如果DADK版本过低，则自动更新
	@echo "dadk version $(DADK_VERSION) installed"
# 如果DADK版本过低，则自动更新
ifneq ($(shell printf '%s\n%s' "$(DADK_VERSION)" "$(MIN_DADK_VERSION)" | sort -V | head -n1), $(MIN_DADK_VERSION))
	@echo "dadk version is too low, please update to $(MIN_DADK_VERSION) or higher version"
	@echo "Updating dadk..."
	cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v$(MIN_DADK_VERSION) || (echo "dadk update failed" && exit 1)
	@echo "dadk updated"
endif
endif

DADK = $(shell which dadk) --manifest $(DADK_MANIFEST_PATH)

.PHONY: dadk_run
dadk_run: install_dadk
	mkdir -p $(DADK_CACHE_DIR)
	$(DADK) user build -w $(ROOT_PATH)
	$(DADK) user install -w $(ROOT_PATH)

.PHONY: dadk_clean
dadk_clean: install_dadk
	@echo dadk_clean

all:
	mkdir -p $(SYSROOT_DIR)
	
	$(MAKE) dadk_run
	$(MAKE) copy_sysconfig

	@echo 用户态程序编译完成

.PHONY: copy_sysconfig
copy_sysconfig:
	cp -r sysconfig/* $(SYSROOT_DIR)


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
