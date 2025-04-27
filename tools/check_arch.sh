#!/bin/bash

BASE_PATH=$(pwd)
# 定义错误信息
ARCH_MISMATCH_ERROR="Error: ARCH in env.mk does not match arch in dadk-manifest.toml"

if [ -z "$ARCH" ]; then
    echo "Error: ARCH environment variable is not set." >&2
    exit 1
fi


# Check if ROOT_PATH is set
if [ -n "$ROOT_PATH" ]; then
    CHECK_PATH="$ROOT_PATH"
else
    # Check if the current directory name is "tools"
    if [ "$(basename "$BASE_PATH")" = "tools" ]; then
        # Try the parent directory's dadk-manifest
        CHECK_PATH=$(dirname "$BASE_PATH")/
    else
        # Otherwise, check the current directory
        CHECK_PATH="$BASE_PATH"
    fi
fi

echo "Checking $CHECK_PATH"


# 读取dadk-manifest.toml文件中的arch字段
DADK_ARCH=$(grep -oP '(?<=arch = ")[^"]+' $CHECK_PATH/dadk-manifest.toml)

# 检查arch字段是否为x86_64
if [ "$ARCH" != $DADK_ARCH ]; then
    echo "$ARCH_MISMATCH_ERROR" >&2
    exit 1
else
    echo "Arch check passed."
    exit 0
fi
