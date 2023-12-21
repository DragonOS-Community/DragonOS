# 导入环境变量
include env.mk


export ROOT_PATH=$(shell pwd)

SUBDIRS = kernel user tools build-scripts



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



# 检查是否需要进行fmt --check
# 解析命令行参数  
FMT_CHECK?=0

ifeq ($(FMT_CHECK), 1)
	FMT_CHECK=--check
else
	FMT_CHECK=
endif


.PHONY: all 
all: kernel user


.PHONY: kernel
kernel:
	mkdir -p bin/kernel/
	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
	$(MAKE) -C ./kernel all ARCH=$(ARCH) || (sh -c "echo 内核编译失败" && exit 1)
	
.PHONY: user
user:

	@if [ -z $$DragonOS_GCC ]; then echo "\033[31m  [错误]尚未安装DragonOS交叉编译器, 请使用tools文件夹下的build_gcc_toolchain.sh脚本安装  \033[0m"; exit 1; fi
	$(MAKE) -C ./user all ARCH=$(ARCH) || (sh -c "echo 用户程序编译失败" && exit 1)

.PHONY: clean
clean:
	@list='$(SUBDIRS)'; for subdir in $$list; do \
		echo "Clean in dir: $$subdir";\
		cd $$subdir && $(MAKE) clean;\
		cd .. ;\
	done

.PHONY: ECHO
ECHO:
	@echo "$@"

cppcheck-xml: 
	cppcheck kernel user --platform=unix64 --std=c11 -I user/libs/ -I=kernel/ --force -j $(NPROCS) --xml 2> cppcheck.xml

cppcheck:
	cppcheck kernel user --platform=unix64 --std=c11 -I user/libs/ -I=kernel/ --force -j $(NPROCS)

docs: ECHO
	bash -c "cd docs && make html && cd .."

clean-docs:
	bash -c "cd docs && make clean && cd .."

gdb:
ifeq ($(ARCH), x86_64)
	rust-gdb -n -x tools/.gdbinit
else
	gdb-multiarch -n -x tools/.gdbinit
endif

# 写入磁盘镜像
write_diskimage:
	@echo "write_diskimage arch=$(ARCH)"
	bash -c "export ARCH=$(ARCH); cd tools && bash grub_auto_install.sh && sudo ARCH=$(ARCH) bash $(ROOT_PATH)/tools/write_disk_image.sh --bios=legacy && cd .."

# 写入磁盘镜像(uefi)
write_diskimage-uefi:
	bash -c "export ARCH=$(ARCH); cd tools && bash grub_auto_install.sh && sudo ARCH=$(ARCH) bash $(ROOT_PATH)/tools/write_disk_image.sh --bios=uefi && cd .."
# 不编译，直接启动QEMU
qemu:
	sh -c "cd tools && bash run-qemu.sh --bios=legacy --display=window && cd .."
# 不编译，直接启动QEMU(UEFI)
qemu-uefi:
	sh -c "cd tools && bash run-qemu.sh --bios=uefi --display=window && cd .."
# 不编译，直接启动QEMU,使用VNC Display作为图像输出
qemu-vnc:
	sh -c "cd tools && bash run-qemu.sh --bios=legacy --display=vnc && cd .."
# 不编译，直接启动QEMU(UEFI),使用VNC Display作为图像输出
qemu-uefi-vnc:
	sh -c "cd tools && bash run-qemu.sh --bios=uefi --display=vnc && cd .."
	
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

# uefi方式启动，使用VNC Display作为图像输出
run-uefi-vnc:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage-uefi || exit 1
	$(MAKE) qemu-uefi-vnc
	
# 编译并启动QEMU，使用VNC Display作为图像输出
run-vnc:
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu-vnc

# 在docker中编译，并启动QEMU
run-docker:
	@echo "使用docker构建并运行"
	sudo bash tools/build_in_docker.sh || exit 1
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu

fmt:
	@echo "格式化代码" 
	FMT_CHECK=$(FMT_CHECK) $(MAKE) fmt -C kernel
	FMT_CHECK=$(FMT_CHECK) $(MAKE) fmt -C user
	FMT_CHECK=$(FMT_CHECK) $(MAKE) fmt -C build-scripts

log-monitor:
	@echo "启动日志监控"
	@sh -c "cd tools/debugging/logmonitor && cargo run --release -- --log-dir $(ROOT_PATH)/logs/ --kernel $(ROOT_PATH)/bin/kernel/kernel.elf" 

.PHONY: update-submodules
update-submodules:
	@echo "更新子模块"
	@git submodule update --recursive
	@git submodule foreach git pull origin master

.PHONY: update-submodules-by-mirror
update-submodules-by-mirror:
	@echo "从镜像更新子模块"
	@git config --global url."https://git.mirrors.dragonos.org.cn/DragonOS-Community/".insteadOf https://github.com/DragonOS-Community/
	@$(MAKE) update-submodules
	@git config --global --unset url."https://git.mirrors.dragonos.org.cn/DragonOS-Community/".insteadOf

help:
	@echo "编译:"
	@echo "  make all -j <n>       - 本地编译，不运行,n为要用于编译的CPU核心数"
	@echo "  make build            - 本地编译，并写入磁盘镜像"
	@echo "  make docker           - Docker编译，并写入磁盘镜像"
	@echo ""
	@echo "编译并运行:"
	@echo "  make run-docker       - Docker编译，写入磁盘镜像，并在QEMU中运行"
	@echo "  make run              - 本地编译，写入磁盘镜像，并在QEMU中运行"
	@echo "  make run-uefi         - 以uefi方式启动运行"
	@echo ""
	@echo "运行:"
	@echo "  make qemu             - 不编译，直接从已有的磁盘镜像启动运行"	
	@echo "  make qemu-uefi        - 不编译，直接从已有的磁盘镜像以UEFI启动运行"	
	@echo ""
	@echo ""
	@echo "注: 对于上述的run, run-uefi, qemu, qemu-uefi命令可以在命令后加上-vnc后缀,来通过vnc连接到DragonOS, 默认会在5900端口运行vnc服务器。如：make run-vnc "
	@echo ""
	@echo "其他:"
	@echo "  make clean            - 清理编译产生的文件"
	@echo "  make fmt              - 格式化代码"
	@echo "  make log-monitor      - 启动日志监控"
	@echo "  make docs             - 生成文档"
	@echo "  make clean-docs       - 清理文档"
	@echo ""
	@echo "  make update-submodules - 更新子模块"
	@echo "  make update-submodules-by-mirror - 从镜像更新子模块"

