#!/bin/bash
set -euo pipefail

ROOT_PATH="${ROOT_PATH:-$(cd "$(dirname "$0")/.." && pwd)}"
ROOTFS_MANIFEST="${ROOTFS_MANIFEST:-default}"
ARCH="${ARCH:-x86_64}"

SRC_MANIFEST="${ROOT_PATH}/config/rootfs-manifests/${ROOTFS_MANIFEST}.toml"
ROOTFS_GENERATED="${ROOT_PATH}/config/rootfs.generated.toml"
DADK_GENERATED="${ROOT_PATH}/dadk-manifest.generated.toml"

if [ ! -f "${SRC_MANIFEST}" ]; then
    echo "Error: rootfs manifest not found: ${SRC_MANIFEST}" >&2
    exit 1
fi

toml_get() {
    local section="$1"
    local key="$2"
    local default_value="${3:-}"
    local value=""

    value=$(
        awk -v section="${section}" -v key="${key}" '
            BEGIN { in_section = 0 }
            /^[[:space:]]*\[/ {
                in_section = ($0 ~ "^[[:space:]]*\\[" section "\\][[:space:]]*$")
                next
            }
            in_section && $0 ~ "^[[:space:]]*" key "[[:space:]]*=" {
                line = $0
                sub(/^[^=]*=[[:space:]]*/, "", line)
                sub(/[[:space:]]*#.*$/, "", line)
                gsub(/^[[:space:]]+|[[:space:]]+$/, "", line)
                gsub(/^"/, "", line)
                gsub(/"$/, "", line)
                print line
                exit
            }
        ' "${SRC_MANIFEST}"
    )

    if [ -z "${value}" ]; then
        echo "${default_value}"
    else
        echo "${value}"
    fi
}

manifest_arch_raw="$(toml_get "metadata" "arch" "")"
if [ -z "${manifest_arch_raw}" ] || [ "${manifest_arch_raw}" = "*" ]; then
    # Missing or wildcard arch means "follow current ARCH".
    manifest_arch="${ARCH}"
else
    manifest_arch="${manifest_arch_raw}"
fi
fs_type="$(toml_get "rootfs" "fs_type" "fat32")"
size="$(toml_get "rootfs" "size" "2G")"
partition="$(toml_get "rootfs" "partition" "mbr")"
base_image="$(toml_get "base" "image" "")"
pull_policy="$(toml_get "base" "pull_policy" "if-not-present")"
user_config_dir="$(toml_get "user" "config_dir" "user/dadk/config")"

if [ "${manifest_arch}" != "${ARCH}" ]; then
    echo "Error: ARCH mismatch, env ARCH=${ARCH}, rootfs manifest arch=${manifest_arch_raw}" >&2
    exit 1
fi

if [ ! -d "${ROOT_PATH}/${user_config_dir}" ]; then
    echo "Error: user config dir not found: ${ROOT_PATH}/${user_config_dir}" >&2
    exit 1
fi

mkdir -p "${ROOT_PATH}/config"

cat > "${ROOTFS_GENERATED}" <<EOF
[metadata]
fs_type = "${fs_type}"
size = "${size}"

[partition]
type = "${partition}"

[base]
image = "${base_image}"
pull_policy = "${pull_policy}"
EOF

cat > "${DADK_GENERATED}" <<EOF
[metadata]
arch = "${manifest_arch}"
hypervisor-config = "config/hypervisor.toml"
rootfs-config = "config/rootfs.generated.toml"
boot-config = "config/boot.toml"
sysroot-dir = "bin/sysroot"
cache-root-dir = "bin/dadk_cache"
user-config-dir = "${user_config_dir}"
app-blocklist-config = "config/app-blocklist.toml"
EOF

echo "Resolved rootfs manifest: ${ROOTFS_MANIFEST}"
echo "  rootfs config -> ${ROOTFS_GENERATED}"
echo "  dadk manifest -> ${DADK_GENERATED}"
