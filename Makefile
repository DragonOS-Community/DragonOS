SUBDIRS = kernel user




export ARCH=__x86_64__
export ROOT_PATH=$(shell pwd)

export DEBUG=DEBUG
export GLOBAL_CFLAGS := -mcmodel=large -fno-builtin -m64  -fno-stack-protector -D $(ARCH) -O1

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif

.PHONY: all
all: kernel user
	


.PHONY: kernel
kernel:
	mkdir -p bin/kernel/
	@list='./kernel'; for subdir in $$list; do \
				echo "make all in $$subdir";\
				cd $$subdir;\
				$(MAKE) all;\
				cd ..;\
		done

.PHONY: user
user:
	mkdir -p bin/user/
	mkdir -p bin/tmp/
	@list='./user'; for subdir in $$list; do \
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