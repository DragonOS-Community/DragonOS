user_sub_dirs = apps

SUBDIR_ROOTS := . 
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.a
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))

DADK_VERSION=$(shell dadk -V | awk 'END {print $$2}')
# 最小的DADK版本
MIN_DADK_VERSION = 0.1.8
DADK_CACHE_DIR = $(ROOT_PATH)/bin/dadk_cache

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
	CC=gcc AS=as AR=ar LD=ld OBJCOPY=objcopy NM=nm cargo install dadk
else
# 如果DADK版本过低，则自动更新
	@echo "dadk version $(DADK_VERSION) installed"
# 如果DADK版本过低，则自动更新
ifneq ($(shell printf '%s\n%s' "$(DADK_VERSION)" "$(MIN_DADK_VERSION)" | sort -V | head -n1), $(MIN_DADK_VERSION))
	@echo "dadk version is too low, please update to $(MIN_DADK_VERSION) or higher version"
	@echo "Updating dadk..."
	CC=gcc AS=as AR=ar LD=ld OBJCOPY=objcopy NM=nm cargo install --git https://git.mirrors.dragonos.org.cn/DragonOS-Community/DADK.git --tag v$(MIN_DADK_VERSION) || (echo "dadk update failed" && exit 1)
	@echo "dadk updated"
endif
endif

.PHONY: dadk_run
dadk_run: install_dadk
	mkdir -p $(DADK_CACHE_DIR)
	dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot build
	dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot install

.PHONY: dadk_clean
dadk_clean: install_dadk
	@echo dadk_clean
# 不运行dadk clean的原因是，把clean的工作交给应用程序自己去做，这样可以节省编译时间
#dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot clean src
#dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot clean target

all:
	mkdir -p $(ROOT_PATH)/bin/sysroot
	
	$(MAKE) dadk_run
	$(MAKE) copy_services

	@echo 用户态程序编译完成

copy_services: dadk_run
	cp -r services/* $(ROOT_PATH)/bin/sysroot/etc/reach/system/


	

.PHONY: clean
clean:
	rm -rf $(GARBAGE)
	$(MAKE) dadk_clean
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

.PHONY: fmt
fmt:
	FMT_CHECK=$(FMT_CHECK) $(MAKE) -C apps fmt
