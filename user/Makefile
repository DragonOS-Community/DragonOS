user_sub_dirs = apps

SUBDIR_ROOTS := . 
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o sys_api_lib *.a
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))


tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(ROOT_PATH)/bin/user

CFLAGS := $(GLOBAL_CFLAGS) -I $(shell pwd)/libs -I $(shell pwd)/libs/libc/src/include -I $(shell pwd)/libs/libc/src/include/export
current_CFLAGS := $(CFLAGS)

DADK_VERSION=$(shell dadk -V | awk 'END {print $$2}')
# 最小的DADK版本
MIN_DADK_VERSION = 0.1.6
DADK_CACHE_DIR = $(ROOT_PATH)/bin/dadk_cache

# 旧版的libc安装路径
OLD_LIBC_INSTALL_PATH=$(ROOT_PATH)/bin/sysroot/usr/old_libc

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
# 之所以在这里临时设置ARCH为空，是因为如果要设置这个环境变量，应当在DADK的配置文件中设置
	ARCH= dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot build
	ARCH= dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot install

.PHONY: dadk_clean
dadk_clean: install_dadk
# 不运行dadk clean的原因是，把clean的工作交给应用程序自己去做，这样可以节省编译时间
#dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot clean src
#dadk --config-dir dadk/config --cache-dir $(DADK_CACHE_DIR) --dragonos-dir $(ROOT_PATH)/bin/sysroot clean target

$(user_sub_dirs): ECHO sys_api_lib 

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"

app: $(user_sub_dirs) dadk_run


all: make_output_dir

	$(MAKE) app

	@echo 用户态程序编译完成

make_output_dir: ECHO
	mkdir -p $(ROOT_PATH)/bin/user/
	mkdir -p $(ROOT_PATH)/bin/tmp/user
	mkdir -p $(ROOT_PATH)/bin/sysroot/usr/include
	mkdir -p $(ROOT_PATH)/bin/sysroot/usr/lib

	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)
# 系统库

sys_api_lib_stage_1: make_output_dir
	@echo Building sys_api_lib...
	$(MAKE) -C libs all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"
	

sys_api_lib: sys_api_lib_stage_1

# 打包系统库
	mkdir -p $(ROOT_PATH)/bin/tmp/user/sys_api_lib_build_tmp
	mkdir -p $(OLD_LIBC_INSTALL_PATH)/include
	mkdir -p $(OLD_LIBC_INSTALL_PATH)/lib
	$(AR) x libs/libc/target/x86_64-unknown-none/release/liblibc.a --output=$(ROOT_PATH)/bin/tmp/user/sys_api_lib_build_tmp 
	$(AR) crvs $(OLD_LIBC_INSTALL_PATH)/lib/libc.a $(shell find ./libs/* -name "*.o") $(shell find $(ROOT_PATH)/bin/tmp/user/sys_api_lib_build_tmp/* -name "*.o")
	rm -rf $(ROOT_PATH)/bin/tmp/user/sys_api_lib_build_tmp
# $(shell find ./libs/* -name "*.o" | xargs -I {} cp {} $(ROOT_PATH)/bin/sysroot/usr/lib/)
	$(shell cp -r $(ROOT_PATH)/user/libs/libc/src/include/export/* $(OLD_LIBC_INSTALL_PATH)/include/)
	$(shell cp -r $(ROOT_PATH)/user/libs/libc/src/arch/x86_64/c*.o $(OLD_LIBC_INSTALL_PATH)/lib/)

.PHONY: clean
clean: 
	rm -rf $(GARBAGE)
	$(MAKE) dadk_clean
	$(MAKE) clean -C libs
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

.PHONY: fmt
fmt:
	FMT_CHECK=$(FMT_CHECK) $(MAKE) -C libs
	FMT_CHECK=$(FMT_CHECK) $(MAKE) -C apps
