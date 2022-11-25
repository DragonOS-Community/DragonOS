SUBDIRS = kernel user

# ifndef $(EMULATOR)
ifeq ($(EMULATOR), )
export EMULATOR=__NO_EMULATION__
endif
# todo: 增加参数，判断是否在QEMU中仿真，若是，则启用该环境变量
# export EMULATOR=__QEMU_EMULATION__

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

export RUSTC=$(shell which rustc)

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
	$(MAKE) -C ./kernel all || (sh -c "echo 内核编译失败" && exit 1)
	
.PHONY: user
user:
	mkdir -p bin/user/
	mkdir -p bin/tmp/user
	$(MAKE) -C ./user all || (sh -c "echo 用户程序编译失败" && exit 1)

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

# 写入磁盘镜像
write_diskimage:
	sudo sh -c "cd tools && bash $(ROOT_PATH)/tools/write_disk_image.sh && cd .."

# 不编译，直接启动QEMU
qemu:
	sh -c "cd tools && bash run-qemu.sh && cd .."

# 编译并写入磁盘镜像
build:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1

# 在docker中编译，并写入磁盘镜像
docker:
	@echo "使用docker构建"
	sudo bash tools/build_in_docker.sh || exit 1

# 编译并启动QEMU
run:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu

# 在docker中编译，并启动QEMU
run-docker:
	@echo "使用docker构建并运行"
	sudo bash tools/build_in_docker.sh || exit 1
	$(MAKE) qemu
