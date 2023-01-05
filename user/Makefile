user_sub_dirs = apps

SUBDIR_ROOTS := . 
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ sys_api_lib *.a
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))


tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(ROOT_PATH)/bin/user

CFLAGS := $(GLOBAL_CFLAGS) -I $(shell pwd)/libs -I $(shell pwd)/libs/libc/src/include
current_CFLAGS := $(CFLAGS)

ECHO:
	@echo "$@"

$(user_sub_dirs): ECHO sys_api_lib

	$(MAKE) -C $@ all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"
	
app: $(user_sub_dirs)

all: app

	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)

	@echo 用户态程序编译完成

make_output_dir: ECHO
	mkdir -p $(ROOT_PATH)/bin/user/
	mkdir -p $(ROOT_PATH)/bin/tmp/user
	mkdir -p $(ROOT_PATH)/bin/sysroot/usr/include
	mkdir -p $(ROOT_PATH)/bin/sysroot/usr/lib

	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)
# 系统库
sys_api_lib: make_output_dir
	$(MAKE) -C libs all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs"
	$(AR) -rcs $(ROOT_PATH)/bin/sysroot/usr/lib/libsysapi.a $(shell find ./libs/* -name "*.o")
	$(shell cp -r $(ROOT_PATH)/user/libs/libc/src/include/* $(ROOT_PATH)/bin/sysroot/usr/include/)
	$(shell cp -r $(ROOT_PATH)/user/libs/libc/src/arch/x86_64/c*.o $(ROOT_PATH)/bin/sysroot/usr/lib/)


clean: 
	rm -rf $(GARBAGE)
	$(MAKE) clean -C libs
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done
	