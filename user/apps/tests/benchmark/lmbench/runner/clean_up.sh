#!/bin/sh
# Clean only resources recorded by init.sh for this run.
SCRIPT_DIR=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
. "$SCRIPT_DIR/env.sh"
STATE_DIR=${LMBENCH_RUN_TMP:-/tmp/lmbench_run}

if [ -f "$STATE_DIR/created_files" ]; then
    while IFS= read -r file_path || [ -n "$file_path" ]; do
        [ -n "$file_path" ] && rm -f "$file_path"
    done < "$STATE_DIR/created_files"
    rm -f "$STATE_DIR/created_files"
fi
if [ -f "$STATE_DIR/mounted_ext4" ]; then
    umount "$LMBENCH_EXT4_DIR" || exit 1
    rm -f "$STATE_DIR/mounted_ext4"
fi
if [ -f "$STATE_DIR/created_ext4_dir" ]; then
    rmdir "$LMBENCH_EXT4_DIR" || exit 1
    rm -f "$STATE_DIR/created_ext4_dir"
fi
if [ -f "$STATE_DIR/created_ext4_image" ]; then
    IFS= read -r image < "$STATE_DIR/created_ext4_image"
    rm -f "$image" "$STATE_DIR/created_ext4_image"
fi
echo 'lmbench test environment cleaned'
