#!/bin/sh
# Prepare fixtures; record ownership so cleanup never unmounts a supplied FS.
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/env.sh"
STATE_DIR=${LMBENCH_RUN_TMP:-/tmp/lmbench_run}

create_ext4_fs() {
    if [ "$LMBENCH_MANAGE_EXT4" = 0 ]; then
        [ -d "$LMBENCH_EXT4_DIR" ] && [ -w "$LMBENCH_EXT4_DIR" ]
        return $?
    fi
    # A caller-provided existing mount is not owned by this run.
    [ -d "$LMBENCH_EXT4_DIR" ] && return 0
    [ "$(id -u)" -eq 0 ] || {
        echo 'Creating an ext4 loop mount requires root privileges' >&2; return 1;
    }
    mkdir -p "$LMBENCH_EXT4_DIR" || return 1
    : > "$STATE_DIR/created_ext4_dir"
    image=${LMBENCH_EXT4_IMAGE:-$SCRIPT_DIR/ext4.img}
    if [ -e "$image" ] || [ -L "$image" ]; then
        echo "Refusing to overwrite existing image: $image" >&2
        return 1
    fi
    (set -C; : > "$image") || return 1
    printf '%s\n' "$image" > "$STATE_DIR/created_ext4_image" || return 1
    dd if=/dev/zero of="$image" bs=1M count=1024 || return 1
    mkfs.ext4 -F "$image" || return 1
    mount -o loop "$image" "$LMBENCH_EXT4_DIR" || return 1
    : > "$STATE_DIR/mounted_ext4"
}

create_one_test_file() {
    file_path=$1
    # Never claim or truncate a pre-existing caller file (including symlinks).
    if [ -e "$file_path" ] || [ -L "$file_path" ]; then
        echo "Refusing to overwrite existing fixture: $file_path" >&2
        return 1
    fi
    (set -C; : > "$file_path") || return 1
    printf '%s\n' "$file_path" >> "$STATE_DIR/created_files" || {
        rm -f "$file_path"; return 1;
    }
    dd if=/dev/zero of="$file_path" bs=1M count=64 || return 1
    [ "$(wc -c < "$file_path")" -eq 67108864 ]
}

main() {
    mkdir -p "$STATE_DIR" "$LMBENCH_TMP_DIR" || return 1
    create_ext4_fs || return 1
    if [ "$LMBENCH_CREATE_TEST_FILES" = 1 ]; then
        for base in "$LMBENCH_EXT4_DIR" "$LMBENCH_TMP_DIR"; do
            create_one_test_file "$base/$LMBENCH_ZERO_FILE" || return 1
            create_one_test_file "$base/$LMBENCH_TEST_FILE" || return 1
        done
    fi
    # Upstream lat_proc and lat_unix_connect use this fixed directory.
    mkdir -p /var/tmp/lmbench || return 1
    if [ ! -e /var/tmp/lmbench/hello ]; then
        printf '%s\n' /var/tmp/lmbench/hello >> "$STATE_DIR/created_files" || return 1
        cp "$LMBENCH_BIN_DIR/hello" /var/tmp/lmbench/hello || return 1
        chmod +x /var/tmp/lmbench/hello || return 1
    fi
}

if main "$@"; then
    echo 'lmbench test environment initialized'
else
    echo 'lmbench test environment initialization failed' >&2
    exit 1
fi
