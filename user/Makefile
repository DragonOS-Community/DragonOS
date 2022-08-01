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

$(user_sub_dirs): ECHO

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"

other_dirs: $(user_sub_dirs)

all: sys_api_lib
	
	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)
	
# 	在成功编译系统库之后，开始编译用户态其他文件
	$(MAKE) other_dirs

# 系统库
sys_api_lib:
	$(MAKE) -C libs all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"



clean: 
	rm -rf $(GARBAGE)
	