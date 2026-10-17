#!/bin/bash
# SPDX-License-Identifier: GPL-2.0
#
# 将 Ubuntu 24.04 (noble) 官方 nodejs 二进制包及其运行时依赖
# 解包到 DADK_CURRENT_BUILD_DIR，形成可直接安装进 DragonOS rootfs 的
# /usr 布局（usr/bin, usr/lib, usr/share/nodejs, ...）。
#
# 依赖宿主机工具：curl、dpkg-deb（缺失时回退到 ar + tar）。
# 可通过 NODEJS_UBUNTU_MIRROR 覆盖下载镜像（默认 USTC）。
set -euo pipefail

MIRROR="${NODEJS_UBUNTU_MIRROR:-http://mirrors.ustc.edu.cn/ubuntu}"

# 固定版本列表（noble），保证构建可复现。
# 该组合已在 DragonOS x86_64 上验证：node -v、脚本执行、事件循环均正常。
DEBS=(
    # nodejs 解释器与核心库
    "pool/universe/n/nodejs/nodejs_18.19.1+dfsg-6ubuntu5_amd64.deb|nodejs_18.19.1+dfsg-6ubuntu5_amd64.deb"
    "pool/universe/n/nodejs/libnode109_18.19.1+dfsg-6ubuntu5_amd64.deb|libnode109_18.19.1+dfsg-6ubuntu5_amd64.deb"
    # 内置模块（REPL/fetch 等运行需要）
    "pool/universe/a/acorn/node-acorn_8.8.1+ds+~cs25.17.7-2_all.deb|node-acorn_8.8.1+ds+~cs25.17.7-2_all.deb"
    "pool/universe/n/node-cjs-module-lexer/node-cjs-module-lexer_1.2.3+dfsg-1_all.deb|node-cjs-module-lexer_1.2.3+dfsg-1_all.deb"
    "pool/universe/n/node-undici/node-undici_5.26.3+dfsg1+~cs23.10.12-2_all.deb|node-undici_5.26.3+dfsg1+~cs23.10.12-2_all.deb"
    # 运行时库依赖
    "pool/main/libu/libuv1/libuv1t64_1.48.0-1.1build1_amd64.deb|libuv1t64_1.48.0-1.1build1_amd64.deb"
    "pool/main/c/c-ares/libcares2_1.27.0-1.0ubuntu1_amd64.deb|libcares2_1.27.0-1.0ubuntu1_amd64.deb"
    "pool/main/b/brotli/libbrotli1_1.1.0-2build2_amd64.deb|libbrotli1_1.1.0-2build2_amd64.deb"
    "pool/main/n/nghttp2/libnghttp2-14_1.59.0-1build4_amd64.deb|libnghttp2-14_1.59.0-1build4_amd64.deb"
    "pool/main/i/icu/libicu74_74.2-1ubuntu3_amd64.deb|libicu74_74.2-1ubuntu3_amd64.deb"
    "pool/main/o/openssl/libssl3t64_3.0.13-0ubuntu3_amd64.deb|libssl3t64_3.0.13-0ubuntu3_amd64.deb"
    "pool/main/g/gcc-14/libstdc++6_14-20240412-0ubuntu1_amd64.deb|libstdc++6_14-20240412-0ubuntu1_amd64.deb"
    "pool/main/g/gcc-14/libgcc-s1_14-20240412-0ubuntu1_amd64.deb|libgcc-s1_14-20240412-0ubuntu1_amd64.deb"
    "pool/main/z/zlib/zlib1g_1.3.dfsg-3.1ubuntu2_amd64.deb|zlib1g_1.3.dfsg-3.1ubuntu2_amd64.deb"
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
        echo "[nodejs_ubuntu2404] downloading ${name}"
        curl -fSL --retry 3 -o "${cached}" "${MIRROR}/${path}"
    fi
    extract_deb "${cached}"
done

echo "[nodejs_ubuntu2404] repacked into ${OUT_DIR}"
