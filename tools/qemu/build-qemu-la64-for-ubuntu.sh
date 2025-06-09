#!/bin/bash
set -e

SCRIPT_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
# 默认参数
QEMU_VERSION="9.2.1"
TARGET_LIST="loongarch64-softmmu"
USE_MIRROR=0
DEST_DIR="${HOME}/opt/qemu-${QEMU_VERSION}"
SOURCE_PACKAGES_DIR="$SCRIPT_DIR/source_packages"
BUILD_DIR="$SCRIPT_DIR/build_dir"

SUDO=sudo
FORCE=0

# 检查是否为root用户
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
fi

# 参数解析
while [[ $# -gt 0 ]]; do
    case "$1" in
        --version)
            QEMU_VERSION="$2"
            shift 2
            DEST_DIR="${HOME}/opt/qemu-${QEMU_VERSION}"  # 更新默认路径
            ;;
        --target-list)
            TARGET_LIST="$2"
            shift 2
            ;;
        --use-mirror)
            USE_MIRROR=1
            shift
            ;;
        --dest-dir)
            DEST_DIR="$2"
            shift 2
            ;;
        -f|--force)
            FORCE=1
            shift
            ;;
        *)
            echo "未知参数: $1"
            exit 1
            ;;
    esac
done

# 检查是否已存在qemu-system-loongarch64
QEMU_BINARY="${DEST_DIR}/bin/qemu-system-loongarch64"
if [ -f "$QEMU_BINARY" ] && [ "$FORCE" -eq 0 ]; then
    echo "检测到已存在 qemu-system-loongarch64 在 ${QEMU_BINARY}"
    echo "如需强制重新构建，请使用 -f 或 --force 参数"
    exit 1
fi

# 依赖检查函数
check_dependency() {
    if ! dpkg -l | grep -q "^ii  $1 "; then
        echo "安装依赖: $1"
        ${SUDO} apt-get install -y "$1"
    fi
}

# 镜像源设置
if [[ $USE_MIRROR -eq 1 ]]; then
    echo "使用国内镜像源..."
    # APT镜像
    ${SUDO} sed -i \
        's|//.*archive.ubuntu.com|//mirrors.ustc.edu.cn|g' \
        /etc/apt/sources.list
    
    # PyPI镜像
    PIP_MIRROR="https://pypi.mirrors.ustc.edu.cn/simple"
else
    PIP_MIRROR="https://pypi.org/simple"
fi

# 更新源
${SUDO} apt-get update

# 安装基础依赖
check_dependency build-essential
check_dependency ninja-build
check_dependency meson
check_dependency pkg-config
check_dependency libglib2.0-dev
check_dependency libslirp-dev
check_dependency wget
check_dependency git

# Python环境配置
if ! command -v python3 &> /dev/null; then
    ${SUDO} apt-get install -y python3 python3-pip
elif ! command -v pip3 &> /dev/null; then
    ${SUDO} apt-get install -y python3-pip
fi

if ! pip3 show tomli &> /dev/null; then
    pip3 install --user -i $PIP_MIRROR tomli
fi

# 创建目录结构
mkdir -p "$DEST_DIR"
mkdir -p $SOURCE_PACKAGES_DIR
mkdir -p $BUILD_DIR

# 下载源码包
QEMU_TAR="qemu-${QEMU_VERSION}.tar.xz"
QEMU_SRC_DIR="$SOURCE_PACKAGES_DIR/qemu-${QEMU_VERSION}"

if [ ! -f "$SOURCE_PACKAGES_DIR/${QEMU_TAR}" ]; then
    echo "正在下载QEMU源码包..."
    wget "https://download.qemu.org/${QEMU_TAR}" -O "$SOURCE_PACKAGES_DIR/${QEMU_TAR}"
fi

# 解压源码
if [ ! -d "${QEMU_SRC_DIR}" ]; then
    echo "解压QEMU源码..."
    tar xf "$SOURCE_PACKAGES_DIR/${QEMU_TAR}" -C $SOURCE_PACKAGES_DIR
fi

# 配置构建目录
BUILD_DIR="$BUILD_DIR/qemu-${QEMU_VERSION}_${TARGET_LIST//,/}"
mkdir -p "${BUILD_DIR}"

pushd $(pwd)

cd "${BUILD_DIR}"

# 运行配置
echo "配置编译参数..."
"${QEMU_SRC_DIR}/configure" \
    --prefix="$DEST_DIR" \
    --enable-slirp \
    --target-list="$TARGET_LIST" \
    --enable-kvm

# 编译和安装
echo "开始编译（使用$(nproc)线程）..."
make -j "$(nproc)"
make install

popd
# 清理
rm -rf "./${BUILD_DIR}"
rm -rf "${QEMU_SRC_DIR}"

echo -e "\n编译完成！安装路径: ${DEST_DIR}"
echo -e "运行以下命令使用QEMU："
echo -e "   或者将其添加到你的shell配置文件中（例如~/.bashrc或~/.zshrc）："
echo "export PATH=\"${DEST_DIR}/bin:\$PATH\""
