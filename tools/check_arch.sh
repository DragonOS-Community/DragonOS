#!/bin/bash

BASE_PATH=$(pwd)
# 定义错误信息
ARCH_MISMATCH_ERROR="Error: ARCH in env.mk does not match arch in rootfs manifest"

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


ROOTFS_MANIFEST=${ROOTFS_MANIFEST:=default}
ROOTFS_MANIFEST_PATH="${CHECK_PATH}/config/rootfs-manifests/${ROOTFS_MANIFEST}.toml"

if [ ! -f "${ROOTFS_MANIFEST_PATH}" ]; then
    echo "Error: rootfs manifest not found: ${ROOTFS_MANIFEST_PATH}" >&2
    exit 1
fi

DADK_ARCH=$(awk '
    BEGIN { in_meta = 0 }
    /^\[metadata\]/ { in_meta = 1; next }
    /^\[/ { in_meta = 0 }
    in_meta && /^[[:space:]]*arch[[:space:]]*=/ {
        line = $0
        sub(/^[^=]*=[[:space:]]*/, "", line)
        gsub(/"/, "", line)
        gsub(/[[:space:]]/, "", line)
        print line
        exit
    }
' "${ROOTFS_MANIFEST_PATH}")

if [ -z "$DADK_ARCH" ]; then
    echo "Error: Failed to parse arch from manifest." >&2
    exit 1
fi

# 检查arch字段是否为x86_64
if [ "$ARCH" != $DADK_ARCH ]; then
    echo "$ARCH_MISMATCH_ERROR" >&2
    exit 1
else
    echo "Arch check passed."
    exit 0
fi
