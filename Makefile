SUBDIRS = kernel user

# ifndef $(EMULATOR)
ifeq ($(EMULATOR), )
export EMULATOR=__NO_EMULATION__
endif

# 计算cpu核心数
NPROCS:=1
OS:=$(shell uname -s)

ifeq ($(OS),Linux)
  NPROCS:=$(shell grep -c ^processor /proc/cpuinfo)
endif
ifeq ($(OS),Darwin) # Assume Mac OS X
  NPROCS:=$(shell system_profiler | awk '/Number Of CPUs/{print $4}{next;}')
endif

export ARCH=__x86_64__
export ROOT_PATH=$(shell pwd)

export DEBUG=DEBUG
export GLOBAL_CFLAGS := -mcmodel=large -fno-builtin -m64  -fno-stack-protector -D $(ARCH) -D $(EMULATOR) -O1

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif

export CC=gcc

.PHONY: all
all: kernel user


.PHONY: kernel
kernel:
	mkdir -p bin/kernel/
	@list='./kernel'; for subdir in $$list; do \
				echo "make all in $$subdir";\
				cd $$subdir;\
				$(MAKE) all;\
				if [ "$$?" != "0" ]; then\
					echo "内核编译失败";\
					exit 1;\
				fi;\
				cd ..;\
		done

.PHONY: user
user:
	mkdir -p bin/user/
	mkdir -p bin/tmp/user
	@list='./user'; for subdir in $$list; do \
    		echo "make all in $$subdir";\
    		cd $$subdir;\
    		$(MAKE) all;\
			if [ "$$?" != "0" ]; then\
				echo "用户态程序编译失败";\
				exit 1;\
			fi;\
    		cd ..;\
	done

.PHONY: clean
clean:
	@list='$(SUBDIRS)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

cppcheck-xml: 
	cppcheck kernel user --platform=unix64 --std=c11 -I user/libs/ -I=kernel/ --force -j $(NPROCS) --xml 2> cppcheck.xml

cppcheck:
	cppcheck kernel user --platform=unix64 --std=c11 -I user/libs/ -I=kernel/ --force -j $(NPROCS)

gdb:
	gdb -n -x tools/.gdbinit
