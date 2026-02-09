#!/bin/busybox sh
set -u

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd)
cd "$SCRIPT_DIR" || exit 1

if [ ! -e /dev/fuse ]; then
    echo "[WARN] /dev/fuse 不存在，测试大概率会失败。"
fi

pass=0
fail=0
total=0
failed_tests=""
found_any=0

for t in ./test_fuse_*; do
    if [ ! -e "$t" ]; then
        continue
    fi
    if [ ! -x "$t" ]; then
        continue
    fi

    found_any=1
    total=$((total + 1))
    name=$(basename "$t")

    echo "===== RUN ${name} ====="
    "$t"
    rc=$?
    if [ "$rc" -eq 0 ]; then
        echo "===== PASS ${name} ====="
        pass=$((pass + 1))
    else
        echo "===== FAIL ${name} (rc=${rc}) ====="
        fail=$((fail + 1))
        failed_tests="${failed_tests}\n  - ${name}(rc=${rc})"
    fi
    echo
done

if [ "$found_any" -eq 0 ]; then
    echo "[ERROR] 当前目录未找到可执行的 test_fuse_*"
    exit 1
fi

echo "===== SUMMARY ====="
echo "TOTAL: ${total}"
echo "PASS: ${pass}"
echo "FAIL: ${fail}"

if [ "$fail" -ne 0 ]; then
    echo "FAILED TESTS:"
    printf "%b\n" "$failed_tests"
    exit 1
fi

echo "ALL test_fuse_* PASSED"
