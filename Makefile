SUBDIRS = kernel user




export ARCH=x86_64
export ROOT_PATH=$(shell pwd)

export DEBUG=DEBUG
export GLOBAL_CFLAGS := -mcmodel=large -fno-builtin -m64  -O0 -fno-stack-protector -D $(ARCH) 

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif

.PHONY: all
all:
	mkdir -p bin/kernel/
	mkdir -p bin/user/
	@list='$(SUBDIRS)'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		 $(MAKE) all;\
    		cd ..;\
    done

.PHONY: clean
clean:
	@list='$(SUBDIRS)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

gdb:
	gdb -n -x tools/.gdbinit