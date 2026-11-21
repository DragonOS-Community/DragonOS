#!/bin/busybox sh
# DragonOS bubblewrap 功能测试运行器
# 读取 whitelist.txt，逐个执行 tests/ 下的 *_test 脚本，按 TAP 协议判断通过/失败，最终输出带颜色的汇总结果

source /etc/profile

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TESTS_DIR="$SCRIPT_DIR/tests"
WHITELIST="$SCRIPT_DIR/whitelist.txt"
TIMEOUT="${TEST_TIMEOUT:-10}"

GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m'

pass=0
fail=0
skip=0
total=0
fail_list=""

run_one_test() {
    _name="$1"
    _test_file="$TESTS_DIR/$_name"

    # 测试脚本不存在，跳过
    if [ ! -x "$_test_file" ]; then
        printf "${YELLOW}SKIP${NC} %s # 脚本不存在\n" "$_name"
        skip=$((skip + 1))
        return
    fi

    # 优先使用 timeout 命令防止卡死，没有 timeout 则直接执行
    if command -v timeout >/dev/null 2>&1; then
        timeout "$TIMEOUT" "$_test_file" >/tmp/bwrap-test-out.txt 2>/tmp/bwrap-test-err.txt
        _rc=$?
        if [ "$_rc" -eq 124 ]; then
            printf "${RED}FAIL${NC} %s # 超时 (%ds)\n" "$_name" "$TIMEOUT"
            fail=$((fail + 1))
            fail_list="$fail_list $_name(超时)"
            return
        fi
    else
        "$_test_file" >/tmp/bwrap-test-out.txt 2>/tmp/bwrap-test-err.txt
        _rc=$?
    fi

    # 检查测试输出最后一行是否为 TAP 格式的测试计划 "1..N"
    # TAP 协议：每个测试脚本以 "1..N" 结尾表示计划运行 N 个子测试，所有 "ok" 行表示通过
    _lastline="$(tail -1 /tmp/bwrap-test-out.txt)"
    if echo "$_lastline" | grep -q '^1\.\.'; then
        _n_ok="$(grep -c '^ok ' /tmp/bwrap-test-out.txt)"
        _n_skip="$(grep -c '^ok .*# SKIP' /tmp/bwrap-test-out.txt)"
        printf "${GREEN}PASS${NC} %s (通过=%d 跳过=%d)\n" "$_name" "$_n_ok" "$_n_skip"
        pass=$((pass + 1))
    else
        # 输出不符合 TAP 格式，视为失败，打印 stderr 前 3 行辅助排查
        printf "${RED}FAIL${NC} %s (退出码=%d)\n" "$_name" "$_rc"
        grep -v '^+' /tmp/bwrap-test-err.txt 2>/dev/null | head -3
        fail=$((fail + 1))
        fail_list="$fail_list $_name"
    fi
}

# 前置检查：白名单和测试目录必须存在
if [ ! -f "$WHITELIST" ]; then
    printf "${RED}致命错误${NC} 白名单不存在: %s\n" "$WHITELIST"
    exit 1
fi

if [ ! -d "$TESTS_DIR" ]; then
    printf "${RED}致命错误${NC} 测试目录不存在: %s\n" "$TESTS_DIR"
    exit 1
fi

# 解析白名单：跳过 # 注释行和空行，收集要运行的测试名
test_names=""
test_count=0
while IFS= read -r line; do
    case "$line" in
        '#'*|'') continue ;;
    esac
    test_names="$test_names $line"
    test_count=$((test_count + 1))
done < "$WHITELIST"

printf "# 开始运行 %d 个 bubblewrap 测试，超时设置 %ds\n\n" "$test_count" "$TIMEOUT"

# 逐个执行白名单中的测试
total=0
for name in $test_names; do
    total=$((total + 1))
    run_one_test "$name"
done

# 输出汇总结果
echo ""
if [ "$fail" -eq 0 ]; then
    printf "${GREEN}# 全部通过: %d/%d${NC}\n" "$pass" "$total"
else
    printf "${RED}# 结果: 通过=%d 失败=%d 跳过=%d 总计=%d${NC}\n" "$pass" "$fail" "$skip" "$total"
    printf "${RED}# 失败列表:%s${NC}\n" "$fail_list"
fi

# 有失败则返回非零退出码，供上层（DADK/CI）判断
if [ "$fail" -gt 0 ]; then
    exit 1
fi
exit 0
