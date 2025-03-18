# uboot版本
UBOOT_VERSION="v2023.10"
RISCV64_UBOOT_PATH="../tools/arch/riscv64/u-boot-${UBOOT_VERSION}-riscv64"

if [ ! -d ${RISCV64_UBOOT_PATH} ]; then
    echo "正在下载u-boot..."
    uboot_tar_name="u-boot-${UBOOT_VERSION}-riscv64.tar.xz"
    
    uboot_parent_path=$(dirname ${RISCV64_UBOOT_PATH}) || (echo "获取riscv u-boot 版本 ${UBOOT_VERSION} 的父目录失败" && exit 1)

    if [ ! -f ${uboot_tar_name} ]; then
        wget https://mirrors.dragonos.org.cn/pub/third_party/u-boot/${uboot_tar_name} || (echo "下载riscv u-boot 版本 ${UBOOT_VERSION} 失败" && exit 1)
    fi
    echo "下载完成"
    echo "正在解压u-boot到 '$uboot_parent_path'..."
    mkdir -p $uboot_parent_path
    tar xvf u-boot-${UBOOT_VERSION}-riscv64.tar.xz -C ${uboot_parent_path} || (echo "解压riscv u-boot 版本 ${UBOOT_VERSION} 失败" && exit 1)
    echo "解压完成"
    rm -rf u-boot-${UBOOT_VERSION}-riscv64.tar.xz
fi
echo "riscv u-boot 版本 ${UBOOT_VERSION} 已经安装"

qemu-system-riscv64 -machine virt -kernel ../tools/arch/riscv64/u-boot-v2023.10-riscv64/u-boot.bin \
                    -m 512M -nographic -smp 2,cores=2,threads=1,sockets=1 -bios default \
                    -no-reboot -device virtio-net-device,netdev=net -netdev user,id=net \
                    -rtc base=utc \
                    -drive file=../bin/riscv64/disk.img,if=none,format=raw,id=x1 \
                    -device virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1 -s