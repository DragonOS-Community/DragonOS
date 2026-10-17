#!/bin/bash
# SPDX-License-Identifier: GPL-2.0
#
# 将 Ubuntu 24.04 (noble) 官方 python3 二进制包及其运行时依赖
# 解包到 DADK_CURRENT_BUILD_DIR，形成可直接安装进 DragonOS rootfs 的
# /usr 布局（usr/bin, usr/lib, ...）。
#
# 依赖宿主机工具：curl、dpkg-deb（缺失时回退到 ar + tar）。
# 可通过 PYTHON3_UBUNTU_MIRROR 覆盖下载镜像（默认 USTC）。
set -euo pipefail

MIRROR="${PYTHON3_UBUNTU_MIRROR:-http://mirrors.ustc.edu.cn/ubuntu}"

# 固定版本列表（noble），保证构建可复现。
# 结构："<pool 相对路径>"（x86_64 已验证组合）
DEBS=(
    # python3 解释器与标准库
    "pool/main/p/python3.12/python3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb|python3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb"
    "pool/main/p/python3.12/libpython3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb|libpython3.12-minimal_3.12.3-1ubuntu0.17_amd64.deb"
    "pool/main/p/python3.12/libpython3.12-stdlib_3.12.3-1ubuntu0.17_amd64.deb|libpython3.12-stdlib_3.12.3-1ubuntu0.17_amd64.deb"
    "pool/main/p/python3.12/python3.12_3.12.3-1ubuntu0.17_amd64.deb|python3.12_3.12.3-1ubuntu0.17_amd64.deb"
    # 运行时库依赖（glibc sysroot 未包含的部分）
    "pool/main/e/expat/libexpat1_2.6.1-2build1_amd64.deb|libexpat1_2.6.1-2build1_amd64.deb"
    "pool/main/z/zlib/zlib1g_1.3.dfsg-3.1ubuntu2_amd64.deb|zlib1g_1.3.dfsg-3.1ubuntu2_amd64.deb"
    "pool/main/libf/libffi/libffi8_3.4.6-1build1_amd64.deb|libffi8_3.4.6-1build1_amd64.deb"
    "pool/main/x/xz-utils/liblzma5_5.6.1+really5.4.5-1_amd64.deb|liblzma5_5.6.1+really5.4.5-1_amd64.deb"
    "pool/main/libx/libxcrypt/libcrypt1_4.4.36-4build1_amd64.deb|libcrypt1_4.4.36-4build1_amd64.deb"
    "pool/main/n/ncurses/libtinfo6_6.4+20240113-1ubuntu2_amd64.deb|libtinfo6_6.4+20240113-1ubuntu2_amd64.deb"
    "pool/main/n/ncurses/libncursesw6_6.4+20240113-1ubuntu2_amd64.deb|libncursesw6_6.4+20240113-1ubuntu2_amd64.deb"
    "pool/main/r/readline/libreadline8t64_8.2-4build1_amd64.deb|libreadline8t64_8.2-4build1_amd64.deb"
    "pool/main/u/util-linux/libuuid1_2.39.3-9ubuntu6_amd64.deb|libuuid1_2.39.3-9ubuntu6_amd64.deb"
    "pool/main/o/openssl/libssl3t64_3.0.13-0ubuntu3_amd64.deb|libssl3t64_3.0.13-0ubuntu3_amd64.deb"
)

OUT_DIR="${DADK_CURRENT_BUILD_DIR:?DADK_CURRENT_BUILD_DIR is not set}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CACHE_DIR="${SCRIPT_DIR}/.deb-cache"
mkdir -p "${OUT_DIR}" "${CACHE_DIR}"

extract_deb() {
    local deb="$1"
    if command -v dpkg-deb >/dev/null 2>&1; then
        dpkg-deb -x "${deb}" "${OUT_DIR}"
    else
        # deb 是 ar 归档；兼容无 dpkg-deb 的构建环境
        ar p "${deb}" data.tar.xz | tar -xJ -C "${OUT_DIR}"
    fi
}

for entry in "${DEBS[@]}"; do
    path="${entry%%|*}"
    name="${entry##*|}"
    cached="${CACHE_DIR}/${name}"
    if [ ! -s "${cached}" ]; then
        echo "[python3_ubuntu2404] downloading ${name}"
        curl -fSL --retry 3 -o "${cached}" "${MIRROR}/${path}"
    fi
    extract_deb "${cached}"
done

# deb 包的 update-alternatives 符号链接由 postinst 创建，离线解包时需要手工建立
mkdir -p "${OUT_DIR}/usr/bin"
ln -sf python3.12 "${OUT_DIR}/usr/bin/python3"
ln -sf /usr/bin/python3.12 "${OUT_DIR}/usr/bin/python"

echo "[python3_ubuntu2404] repacked into ${OUT_DIR}"
