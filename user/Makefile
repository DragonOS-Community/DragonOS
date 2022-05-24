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
	
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		 $(MAKE) all CFLAGS="$(CFLAGS)";\
    		cd ..;\
	done

	
	$(shell if [ ! -e $(tmp_output_dir) ];then mkdir -p $(tmp_output_dir); fi)
	$(shell if [ ! -e $(output_dir) ];then mkdir -p $(output_dir); fi)

	$(MAKE) sys_api_lib
	$(MAKE) shell
# objcopy -I elf64-x86-64 -S -R ".eh_frame" -R ".comment" -O binary sys_api_lib $(ROOT_PATH)/bin/user/init.bin
#objcopy -I elf64-x86-64 -S -R ".eh_frame" -R ".comment" -O elf64-x86-64 sys_api_lib $(ROOT_PATH)/bin/user/init.bin

sys_api_lib:

	ld -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/sys_api_lib $(shell find ./libs -name "*.o")
#ld -b elf64-x86-64 -z muldefs -o sys_api_lib init.o $(shell find . -name "*.o") -T init.lds

shell:
	ld -b elf64-x86-64 -z muldefs -o $(tmp_output_dir)/shell  $(shell find ./apps/shell -name "*.o") $(shell find ./libs -name "*.o")

	objcopy -I elf64-x86-64 -S -R ".eh_frame" -R ".comment" -O elf64-x86-64 $(tmp_output_dir)/shell $(output_dir)/shell.elf
clean: 
	rm -rf $(GARBAGE)
	