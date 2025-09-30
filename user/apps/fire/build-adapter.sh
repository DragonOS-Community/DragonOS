#!/bin/bash
# Fire 容器运行时构建适配脚本
# 用于在 DragonOS 环境中构建 fire

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
FIRE_SOURCE_DIR="${SCRIPT_DIR}/fire-source"
ROOT_PATH="${ROOT_PATH:-/tmp/dragonos-build}"

echo "Fire 构建适配脚本启动..."
echo "脚本目录: ${SCRIPT_DIR}"
echo "源码目录: ${FIRE_SOURCE_DIR}"
echo "ROOT_PATH: ${ROOT_PATH}"

# 检查源码目录是否存在
if [ ! -d "${FIRE_SOURCE_DIR}" ]; then
    echo "错误: Fire 源码目录不存在: ${FIRE_SOURCE_DIR}"
    echo "请确保已正确克隆 fire 源码"
    exit 1
fi

# 进入源码目录
cd "${FIRE_SOURCE_DIR}"

# 检查是否有 Cargo.toml
if [ ! -f "Cargo.toml" ]; then
    echo "错误: 未找到 Cargo.toml 文件"
    exit 1
fi

# 复制我们的构建配置
echo "复制 DragonOS 构建配置..."
mkdir -p .cargo
cp "${SCRIPT_DIR}/.cargo/config.toml" .cargo/

# 设置环境变量
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER="x86_64-linux-musl-gcc"
export CC="x86_64-linux-musl-gcc"
export CXX="x86_64-linux-musl-g++"

# 添加目标架构
echo "添加 musl 目标..."
rustup target add x86_64-unknown-linux-musl || echo "目标已存在"

# 构建项目
echo "开始构建 Fire..."
case "${1:-build}" in
    "build")
        cargo build --release --target x86_64-unknown-linux-musl
        ;;
    "install")
        cargo build --release --target x86_64-unknown-linux-musl
        
        # 安装到 sysroot（不使用 sudo）
        echo "安装 Fire 到 DragonOS sysroot..."
        INSTALL_DIR="${ROOT_PATH}/bin/sysroot"
        mkdir -p "${INSTALL_DIR}/usr/bin"
        mkdir -p "${INSTALL_DIR}/etc/fire"
        mkdir -p "${INSTALL_DIR}/var/lib/fire"
        
        # 查找二进制文件
        BINARY_PATH=$(find target/x86_64-unknown-linux-musl/release -name "fire" -type f | head -1)
        if [ -z "${BINARY_PATH}" ]; then
            echo "错误: 未找到构建的二进制文件"
            exit 1
        fi
        
        # 直接复制，不使用 sudo
        cp "${BINARY_PATH}" "${INSTALL_DIR}/usr/bin/"
        chmod +x "${INSTALL_DIR}/usr/bin/fire"
        
        # 创建默认配置
        cat > "${INSTALL_DIR}/etc/fire/config.toml" << EOF
# Fire 容器运行时配置
root = "/var/lib/fire"
log_level = "info"
EOF
        
        echo "Fire 安装完成！"
        echo "二进制文件: /usr/bin/fire"
        echo "配置文件: /etc/fire/config.toml"
        ;;
    "clean")
        cargo clean
        ;;
    "check")
        cargo check --target x86_64-unknown-linux-musl
        ;;
    *)
        echo "未知命令: $1"
        echo "支持的命令: build, install, clean, check"
        exit 1
        ;;
esac

echo "构建适配脚本完成"