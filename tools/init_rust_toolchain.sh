# 当前脚本用于初始化自定义的Rust工具链
if [ -z "$(which cargo)" ]; then
    echo "尚未安装Rust，请先安装Rust"
    exit 1
fi

WORK_DIR=$(pwd)
RUST_SRC_VERSION=1.66.0
# 初始化bare bone工具链
DRAGONOS_UNKNOWN_ELF_PATH=$(rustc --print sysroot)/lib/rustlib/x86_64-unknown-dragonos
mkdir -p ${DRAGONOS_UNKNOWN_ELF_PATH}/lib
# 设置工具链配置文件
echo   \
"{\
    \"arch\": \"x86_64\",
    \"code-model\": \"kernel\",
    \"cpu\": \"x86-64\",
    \"os\": \"dragonos\",
    \"target-endian\": \"little\",
    \"target-pointer-width\": \"64\",
    \"target-c-int-width\": \"32\",
    \"data-layout\": \"e-m:e-p270:32:32-p271:32:32-p272:64:64-i64:64-f80:128-n8:16:32:64-S128\",
    \"disable-redzone\": true,
    \"features\": \"-mmx,-sse,-sse2,-sse3,-ssse3,-sse4.1,-sse4.2,-3dnow,-3dnowa,-avx,-avx2,+soft-float\",
    \"linker\": \"rust-lld\",
    \"linker-flavor\": \"ld.lld\",
    \"llvm-target\": \"x86_64-unknown-none\",
    \"max-atomic-width\": 64,
    \"panic-strategy\": \"abort\",
    \"position-independent-executables\": true,
    \"relro-level\": \"full\",
    \"stack-probes\": {
      \"kind\": \"inline-or-call\",
      \"min-llvm-version-for-inline\": [
        16,
        0,
        0
      ]
    },
    \"static-position-independent-executables\": true,
    \"supported-sanitizers\": [
      \"kcfi\"
    ],
    \"target-pointer-width\": \"64\"
}" > ${DRAGONOS_UNKNOWN_ELF_PATH}/target.json || exit 1


# echo   \
# "{
#   \"llvm-target\": \"x86_64-unknown-none\",
#   \"data-layout\": \"e-m:e-i64:64-f80:128-n8:16:32:64-S128\",
#   \"arch\": \"x86_64\",
#   \"target-endian\": \"little\",
#   \"target-pointer-width\": \"64\",
#   \"target-c-int-width\": \"32\",
#   \"os\": \"dragonos\",
#   \"linker\": \"rust-lld\",
#   \"linker-flavor\": \"ld.lld\",
#   \"executables\": true,
#   \"features\": \"-mmx,-sse,+soft-float\",
#   \"disable-redzone\": true,
#   \"panic-strategy\": \"abort\"
# }" > ${DRAGONOS_UNKNOWN_ELF_PATH}/target.json || exit 1


# 编译标准库 (仍存在问题，不能编译)
# mkdir -p build || exit 1
# cd build
# if [ ! -d "rust" ]; then
#     git clone -b $RUST_SRC_VERSION https://github.com/rust-lang/rust.git --depth=1 --recursive || exit 1
# fi

# cd rust
# git checkout $RUST_SRC_VERSION
# git submodule update --init --recursive

# cargo clean
# export RUST_COMPILER_RT_ROOT=$(pwd)/src/llvm-project/compiler-rt
# CARGO_PROFILE_RELEASE_DEBUG=0 \
# CARGO_PROFILE_RELEASE_DEBUG_ASSERTIONS=true \
# RUSTC_BOOTSTRAP=1 \
# RUSTFLAGS="-Cforce-unwind-tables=yes -Cembed-bitcode=yes" \
# __CARGO_DEFAULT_LIB_METADATA="stablestd" \
#     ./x.py build --target x86_64-unknown-dragonos || exit 1
