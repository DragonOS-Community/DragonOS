user_sub_dirs = libs

SUBDIR_ROOTS := . 
DIRS := . $(shell find $(SUBDIR_ROOTS) -type d)
GARBAGE_PATTERNS := *.o *.s~ *.s *.S~ *.c~ *.h~ sys_api_lib
GARBAGE := $(foreach DIR,$(DIRS),$(addprefix $(DIR)/,$(GARBAGE_PATTERNS)))


objs := 
CFLAGS := $(GLOBAL_CFLAGS) 
current_CFLAGS := $(CFLAGS) -I $(shell pwd)/libs
all: 
	
	@list='$(user_sub_dirs)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		 $(MAKE) all CFLAGS="$(CFLAGS)";\
    		cd ..;\
	done
	$(MAKE) init.o
	$(MAKE) sys_api_lib
	
# objcopy -I elf64-x86-64 -S -R ".eh_frame" -R ".comment" -O binary sys_api_lib $(ROOT_PATH)/bin/user/init.bin
	objcopy -I elf64-x86-64 -S -R ".eh_frame" -R ".comment" -O elf64-x86-64 sys_api_lib $(ROOT_PATH)/bin/user/init.bin

sys_api_lib: init.o
	

	ld -b elf64-x86-64 -z muldefs -o sys_api_lib init.o  $(shell find . -name "*.o")
#ld -b elf64-x86-64 -z muldefs -o sys_api_lib init.o $(shell find . -name "*.o") -T init.lds

init.o: init.c 
	gcc $(current_CFLAGS) -c init.c -o init.o
clean: 
	rm -rf $(GARBAGE)