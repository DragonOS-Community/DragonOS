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

export DEBUG=DEBUG
export GLOBAL_CFLAGS := -mcmodel=large -fno-builtin -m64  -fno-stack-protector -D $(ARCH) -D $(EMULATOR) -O1

ifeq ($(DEBUG), DEBUG)
GLOBAL_CFLAGS += -g 
endif


export CC=$(DragonOS_GCC)/x86_64-elf-gcc
export LD=ld
export AS=$(DragonOS_GCC)/x86_64-elf-as
export NM=$(DragonOS_GCC)/x86_64-elf-nm
export AR=$(DragonOS_GCC)/x86_64-elf-ar
export OBJCOPY=$(DragonOS_GCC)/x86_64-elf-objcopy
export AR=$(DragonOS_GCC)/x86_64-elf-ar


.PHONY: all 
all: kernel user


.PHONY: kernel
kernel:
	mkdir -p bin/kernel/
	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
	$(MAKE) -C ./kernel all || (sh -c "echo 内核编译失败" && exit 1)
	
.PHONY: user
user:
	mkdir -p bin/user/
	mkdir -p bin/tmp/user
	mkdir -p bin/sysroot/usr/include
	mkdir -p bin/sysroot/usr/lib 
	$(shell cp -r $(shell pwd)/user/libs/libc/src/include/* $(shell pwd)/bin/sysroot/usr/include/)
	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
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
	bash -c "cd tools && bash grub_auto_install.sh && sudo bash $(ROOT_PATH)/tools/write_disk_image.sh --bios=legacy && cd .."

# 写入磁盘镜像(uefi)
write_diskimage-uefi:
	bash -c "cd tools && bash grub_auto_install.sh && sudo bash $(ROOT_PATH)/tools/write_disk_image.sh --bios=uefi && cd .."
# 不编译，直接启动QEMU
qemu:
	sh -c "cd tools && bash run-qemu.sh --bios=legacy && cd .."
# 不编译，直接启动QEMU(UEFI)
qemu-uefi:
	sh -c "cd tools && bash run-qemu.sh --bios=uefi && cd .."
	
# 编译并写入磁盘镜像
build:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1

# 在docker中编译，并写入磁盘镜像
docker:
	@echo "使用docker构建"
	sudo bash tools/build_in_docker.sh || exit 1
	$(MAKE) write_diskimage || exit 1
	
# uefi方式启动
run-uefi:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage-uefi || exit 1
	$(MAKE) qemu-uefi
	
# 编译并启动QEMU
run:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu

# 在docker中编译，并启动QEMU
run-docker:
	@echo "使用docker构建并运行"
	sudo bash tools/build_in_docker.sh || exit 1
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu
