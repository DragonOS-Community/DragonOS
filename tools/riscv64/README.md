# Risc-V 启动发烟测试

支持三条路径：

- `sbi`：OpenSBI → DragonOS raw kernel（默认）。
- `u-boot`：OpenSBI → U-Boot → DragonStub → DragonOS。
- `edk2`：OpenSBI → EDK II → DragonStub → DragonOS。

需要 Linux、DragonOS RISC-V 编译环境，以及以下运行和依赖构建工具：

```sh
sudo apt-get install -y \
  acpica-tools bison build-essential ca-certificates curl device-tree-compiler \
  dosfstools fdisk flex gcc-riscv64-linux-gnu libc6-dev-riscv64-cross git libssl-dev \
  bzip2 mtools nasm python3 python3-dev python3-setuptools \
  qemu-system-misc swig util-linux uuid-dev xz-utils
```

在仓库根目录运行，脚本会编译当前内核，按需下载、校验并缓存 BusyBox 和引导程序:

```sh
git submodule update --init --recursive
tools/riscv64/opensbi-smoke.sh sbi  # 也可选 u-boot 或 edk2
```

也可传入已有产物，跳过内核构建:

```sh
export DRAGONOS_KERNEL_ELF="$PWD/bin/kernel/kernel.elf"
export DRAGONOS_EFI="$PWD/bin/sysroot/efi/boot/bootriscv64.efi"
for mode in sbi u-boot edk2; do
  tools/riscv64/opensbi-smoke.sh "$mode" || exit 1
done
```

默认使用 QEMU 自带 OpenSBI（`-bios default`）、单 hart、2 GiB 内存和新建 FAT
磁盘。脚本先校验内核 ELF 的 RISC-V 架构；进入 BusyBox 后执行 shell 内建命令
`test -r /bin/sh && printf 'DRAGONOS-%s\n' SMOKE-OK`，收到独立一行
`DRAGONOS-SMOKE-OK` 才返回 0。
此测试暂只覆盖进入用户态及 shell 内建命令，不覆盖 fork/exec 外部程序；
外部命令触发的 RISC-V 子进程 RCU 状态异常留待后续修复并恢复覆盖。
退出时清理测试磁盘，保留日志。

`OPENSBI_FIRMWARE` 可指定 OpenSBI `fw_dynamic.bin`；`DRAGONOS_BOOT_TIMEOUT_SECS`
设置启动超时（默认 180 秒）。构建目录、缓存、日志默认在 `bin/riscv64-smoke-*`，
可用 `DRAGONOS_WORK_DIR`、`DRAGONOS_CACHE_DIR`、`DRAGONOS_LOG_DIR` 覆盖。

`.github/workflows/riscv64-smoke.yml` 在 PR、主分支推送和手动触发时运行：
仅构建 riscv64 内核，并使用本次构建的 RISC-V 内核及 EFI
产物测试三条启动路径。启动依赖可缓存，内核每次重新构建；失败会使 CI 失败，
串口日志作为 artifact 保留。
