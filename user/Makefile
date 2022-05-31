user_sub_dirs = libs apps

SUBDIR_ROOTS := . 
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ sys_api_lib
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))


tmp_output_dir=$(ROOT_PATH)/bin/tmp/user
output_dir=$(ROOT_PATH)/bin/user

CFLAGS := $(GLOBAL_CFLAGS) -I $(shell pwd)/libs
current_CFLAGS := $(CFLAGS)
all: 
	
	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)
	
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		 $(MAKE) all CFLAGS="$(CFLAGS)" tmp_output_dir="$(tmp_output_dir)" output_dir="$(output_dir)" sys_libs_dir="$(shell pwd)/libs";\
    		cd ..;\
	done	

# 系统库
sys_api_lib:
	@list='./libs'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		 $(MAKE) all CFLAGS="$(CFLAGS)";\
    		cd ..;\
	done

# ld -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/sys_api_lib $(shell find ./libs -name "*.o")
#ld -b elf64-x86-64 -z muldefs -o sys_api_lib init.o $(shell find . -name "*.o") -T init.lds


clean: 
	rm -rf $(GARBAGE)
	