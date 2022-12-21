user_sub_dirs = apps

SUBDIR_ROOTS := . 
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ sys_api_lib
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))


tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(ROOT_PATH)/bin/user

CFLAGS := $(GLOBAL_CFLAGS) -I $(shell pwd)/libs
current_CFLAGS := $(CFLAGS)

ECHO:
	@echo "$@"

$(user_sub_dirs): ECHO sys_api_lib

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"
	
app: $(user_sub_dirs)

all: app
	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)

	@echo 用户态程序编译完成

# 系统库
sys_api_lib:
	$(MAKE) -C libs all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"



clean: 
	rm -rf $(GARBAGE)
	$(MAKE) clean -C libs
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done
	