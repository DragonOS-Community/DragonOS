# 导入环境变量
include env.mk


export ROOT_PATH=$(shell pwd)
export VMSTATE_DIR=$(ROOT_PATH)/bin/vmstate

# 检测是否在 Nix 环境中
IN_NIX_ENV := $(USING_DRAGONOS_NIX_ENV)

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

# 是否跳过grub自动安装。CI环境或纯nographic运行可以设置为1以节省时间。
SKIP_GRUB ?= 0
# CI环境默认跳过
ifneq ($(CI),)
SKIP_GRUB := 1
endif

ifeq ($(SKIP_GRUB),1)
GRUB_PREPARE_CMD := printf 'Skip grub_auto_install.sh (SKIP_GRUB=1)\n'
GRUB_SKIP_ENV := SKIP_GRUB=1
else
GRUB_PREPARE_CMD := bash grub_auto_install.sh
GRUB_SKIP_ENV :=
endif

ifeq ($(FMT_CHECK), 1)
	FMT_CHECK=--check
else
	FMT_CHECK=
endif

# Check if ARCH matches the arch field in dadk-manifest.toml
check_arch:
	@bash tools/check_arch.sh

# Check if Nix is installed
check_nix:
	@if ! command -v nix >/dev/null 2>&1; then \
		echo ""; \
		echo "错误: Nix 未安装!"; \
		echo ""; \
		echo "请通过以下方式安装 Nix:"; \
		echo "  curl -fsSL https://install.determinate.systems/nix | sh -s -- install"; \
		echo ""; \
		echo "或访问 https://nixos.org/download/ 获取更多安装选项。"; \
		echo ""; \
		exit 1; \
	fi

.PHONY: all
all: kernel user


.PHONY: kernel
kernel: check_arch
	mkdir -p bin/kernel/

	$(MAKE) -C ./kernel all ARCH=$(ARCH) || (sh -c "echo 内核编译失败" && exit 1)

.PHONY: user
user: check_arch
ifeq ($(IN_NIX_ENV),1)
	@echo "⚠️  警告: 在 Nix 环境中使用 'make user' 已被弃用"
	@echo "   请使用: nix run .#rootfs-$(ARCH)"
	@echo ""
	@echo "   正在执行 nix run .#rootfs-$(ARCH)..."
	nix run .#rootfs-$(ARCH)
else
	$(MAKE) -C ./user all ARCH=$(ARCH) || (sh -c "echo 用户程序编译失败" && exit 1)
endif

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


docs: ECHO
	bash -c "cd docs && make html && cd .."

clean-docs:
	bash -c "cd docs && make clean && cd .."

gdb: check_arch
	@if [ -f "$(VMSTATE_DIR)/gdb" ]; then \
		GDB_PORT=$$(cat $(VMSTATE_DIR)/gdb); \
		GDB_INIT_TMP=$$(mktemp); \
		trap "rm -f $$GDB_INIT_TMP" EXIT; \
		echo "连接到GDB端口: $$GDB_PORT"; \
		sed "s/{{GDB_PORT}}/$$GDB_PORT/" tools/.gdbinit > "$$GDB_INIT_TMP"; \
		if [ "$(ARCH)" = "x86_64" ]; then \
			rust-gdb -n -x "$$GDB_INIT_TMP"; \
		elif [ "$(ARCH)" = "loongarch64" ]; then \
			loongarch64-unknown-linux-gnu-gdb -n -x "$$GDB_INIT_TMP"; \
		else \
			gdb-multiarch -n -x "$$GDB_INIT_TMP"; \
		fi \
	else \
		echo "错误: VM未运行或GDB端口未分配"; \
		echo "请先启动VM: make qemu"; \
	fi

# 获取VM状态信息
get-vmstate:
	@if [ -f "$(VMSTATE_DIR)/port" ]; then \
		echo "网络端口: $$(cat $(VMSTATE_DIR)/port)"; \
	else \
		echo "网络端口: VM未运行"; \
	fi
	@if [ -f "$(VMSTATE_DIR)/pid" ]; then \
		echo "进程PID: $$(cat $(VMSTATE_DIR)/pid)"; \
	else \
		echo "进程PID: VM未运行"; \
	fi
	@if [ -f "$(VMSTATE_DIR)/gdb" ]; then \
		echo "GDB端口: $$(cat $(VMSTATE_DIR)/gdb)"; \
	else \
		echo "GDB端口: VM未运行"; \
	fi


# （nix）构建用户程序并生成磁盘镜像
rootfs: check_nix
	@echo "Generating RootFS Disk Image with Nix, default is ext4"
	@echo "To change building image type, change the 'rootfsType' to 'vfat' in flake.nix"
	nix run .#rootfs-x86_64

# 写入磁盘镜像
write_diskimage: check_arch
ifeq ($(IN_NIX_ENV),1)
	@echo "⚠️  警告: 在 Nix 环境中使用 'make write_diskimage' 已被弃用"
	@echo "   请使用: nix run .#rootfs-$(ARCH)"
	@echo ""
	@echo "   正在执行 nix run .#rootfs-$(ARCH)..."
	nix run .#rootfs-$(ARCH)
else
	@echo "write_diskimage arch=$(ARCH)"
	bash -c "export ARCH=$(ARCH); cd tools && $(GRUB_PREPARE_CMD) && sudo DADK=$(DADK) $(GRUB_SKIP_ENV) ARCH=$(ARCH) bash $(ROOT_PATH)/tools/write_disk_image.sh --bios=legacy && cd .."
endif

# 写入磁盘镜像(uefi)
write_diskimage-uefi: check_arch
	bash -c "export ARCH=$(ARCH); cd tools && $(GRUB_PREPARE_CMD) && sudo DADK=$(DADK) $(GRUB_SKIP_ENV) ARCH=$(ARCH) bash $(ROOT_PATH)/tools/write_disk_image.sh --bios=uefi && cd .."
# 不编译，直接启动QEMU
qemu: check_arch
ifeq ($(IN_NIX_ENV),1)
	@echo "ℹ️  在 Nix 环境中启动 QEMU"
	@echo "   使用默认配置无图形化启动 (-nographic)"
	@echo ""
	@echo "   其他配置尚未在 nix qemu run script 实现"
	@echo "   需要自行指定 flag"
	@echo ""
	nix run .#start-$(ARCH)
else
	sh -c "cd tools && bash run-qemu.sh --bios=legacy --display=window && cd .."
endif

# 不编译，直接启动QEMU,不显示图像
qemu-nographic: check_arch
ifeq ($(IN_NIX_ENV),1)
	@echo "ℹ️  在 Nix 环境中启动 QEMU (nographic 模式)"
	@echo "   注意: nix run .#start-$(ARCH) 默认使用非图形模式"
	@echo ""
	nix run .#start-$(ARCH) -- -nographic
else
	sh -c "cd tools && bash run-qemu.sh --bios=legacy --display=nographic && cd .."
endif

# 不编译，直接启动QEMU(UEFI)
qemu-uefi: check_arch
	sh -c "cd tools && bash run-qemu.sh --bios=uefi --display=window && cd .."
# 不编译，直接启动QEMU,使用VNC Display作为图像输出
qemu-vnc: check_arch
	sh -c "cd tools && bash run-qemu.sh --bios=legacy --display=vnc && cd .."
# 不编译，直接启动QEMU(UEFI),使用VNC Display作为图像输出
qemu-uefi-vnc: check_arch
	sh -c "cd tools && bash run-qemu.sh --bios=uefi --display=vnc && cd .."

# 编译并写入磁盘镜像
build: check_arch
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1

# 在docker中编译，并写入磁盘镜像
docker: check_arch
	@echo "使用docker构建"
	sudo bash tools/build_in_docker.sh || exit 1
	$(MAKE) write_diskimage || exit 1

# uefi方式启动
run-uefi: check_arch
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage-uefi || exit 1
	$(MAKE) qemu-uefi

# 编译并启动QEMU
run: check_arch
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu

# uefi方式启动，使用VNC Display作为图像输出
run-uefi-vnc: check_arch
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage-uefi || exit 1
	$(MAKE) qemu-uefi-vnc

# 编译并启动QEMU，使用VNC Display作为图像输出
run-vnc: check_arch
	$(MAKE) all -j $(NPROCS)
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu-vnc

run-nographic: check_arch
ifeq ($(IN_NIX_ENV),1)
	@echo "⚠️  警告: 在 Nix 环境中使用 'make run-nographic' 已被弃用"
	@echo "   请使用: nix run .#yolo-$(ARCH)"
	@echo ""
	@echo "   正在执行 nix run .#yolo-$(ARCH)..."
	nix run .#yolo-$(ARCH)
else
	$(MAKE) all -j $(NPROCS)
	SKIP_GRUB=1 $(MAKE) write_diskimage || exit 1
	# $(MAKE) rootfs
	$(MAKE) qemu-nographic
endif
# 在docker中编译，并启动QEMU
run-docker: check_arch
	@echo "使用docker构建并运行"
	sudo bash tools/build_in_docker.sh || exit 1
	$(MAKE) write_diskimage || exit 1
	$(MAKE) qemu

test-syscall: check_arch
	@echo "构建运行并执行syscall测试"
	bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh enable
	$(MAKE) all -j $(NPROCS)
	@if [ "$(DISK_SAVE_MODE)" = "1" ]; then \
		echo "磁盘节省模式启用，正在清理用户程序构建缓存..."; \
		$(DADK) user clean --level in-src; \
	fi
	SKIP_GRUB=1 $(MAKE) write_diskimage || exit 1
	$(MAKE) qemu-nographic AUTO_TEST=syscall SYSCALL_TEST_DIR=/opt/tests/gvisor &
	sleep 5
	@{ \
		status=0; \
		bash user/apps/tests/syscall/gvisor/monitor_test_results.sh || status=$$?; \
		bash user/apps/tests/syscall/gvisor/toggle_compile_gvisor.sh disable; \
		exit $$status; \
	}

fmt: check_arch
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
	@git submodule update --recursive --init

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
	@echo "VM状态管理:"
	@echo "  make get-vmstate       - 获取VM状态（端口、PID、GDB端口）"
	@echo ""
	@echo "调试:"
	@echo "  make gdb              - 启动GDB调试"
	@echo ""
	@echo "其他:"
	@echo "  make clean            - 清理编译产生的文件"
	@echo "  make fmt              - 格式化代码"
	@echo "  make log-monitor      - 启动日志监控"
	@echo "  make docs             - 生成文档"
	@echo "  make clean-docs       - 清理文档"
	@echo "  make test-syscall     - 构建运行并执行syscall测试"
	@echo "                         - 可通过DISK_SAVE_MODE=1启用磁盘节省模式"
	@echo ""
	@echo "环境变量:"
	@echo "  DISK_SAVE_MODE=1     - 启用磁盘节省模式，在写入磁盘镜像前清理构建缓存"
	@echo ""
	@echo "  make update-submodules - 更新子模块"
	@echo "  make update-submodules-by-mirror - 从镜像更新子模块"
