#!/bin/bash

# gvisor系统调用测试运行脚本
# 用于DragonOS项目

set -o pipefail

SCRIPT_DIR=$(dirname "$(realpath "$0")")
TESTS_DIR="$SCRIPT_DIR/tests"
BLOCKLISTS_DIR="$SCRIPT_DIR/blocklists"
WHITELIST_FILE="$SCRIPT_DIR/whitelist.txt"
RESULTS_DIR="$SCRIPT_DIR/results"
TEMP_DIR="${SYSCALL_TEST_WORKDIR:-/tmp/gvisor_tests}"

# 测试统计
TOTAL_TESTS=0
PASSED_TESTS=0
FAILED_TESTS=0
SKIPPED_TESTS=0

# 颜色定义
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
BLUE='\033[0;34m'
NC='\033[0m'

# 输出文件
FAILED_CASES_FILE="$RESULTS_DIR/failed_cases.txt"
TEST_REPORT_FILE="$RESULTS_DIR/test_report.txt"

print_info() {
    echo -e "${GREEN}[INFO]${NC} $1"
}

print_warn() {
    echo -e "${YELLOW}[WARN]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

print_test() {
    echo -e "${BLUE}[TEST]${NC} $1"
}

# 显示使用帮助
show_help() {
    cat << EOF
用法: $0 [选项] [测试名称模式...]

选项:
  -h, --help           显示此帮助信息
  -l, --list           列出所有可用的测试用例
  -v, --verbose        详细输出模式
  -t, --timeout SEC    设置单个测试的超时时间（默认：300秒）
  -j, --parallel NUM   并行运行的测试数量（默认：1）
  --no-blocklist       忽略所有blocklist文件
  --extra-blocklist DIR 指定额外的blocklist目录
  --no-whitelist       禁用白名单模式，运行所有测试程序
  --whitelist FILE     指定白名单文件路径（默认：whitelist.txt）

说明:
  默认启用白名单模式，只运行whitelist.txt中的测试程序。
  测试用例仍使用blocklist过滤机制。

示例:
  $0                   运行白名单中的测试程序
  $0 socket_test       运行socket_test（如果在白名单中）
  $0 --no-whitelist    运行所有测试程序
  $0 --whitelist my_whitelist.txt  使用自定义白名单文件
  $0 -j 4 -v          使用4个并行进程详细运行白名单中的测试
EOF
}

# 解析命令行参数
parse_args() {
    VERBOSE=false
    TIMEOUT=300
    PARALLEL=1
    USE_BLOCKLIST=true
    USE_WHITELIST=true
    EXTRA_BLOCKLIST_DIRS=""
    TEST_PATTERNS=()
    
    while [[ $# -gt 0 ]]; do
        case $1 in
            -h|--help)
                show_help
                exit 0
                ;;
            -l|--list)
                list_tests
                exit 0
                ;;
            -v|--verbose)
                VERBOSE=true
                shift
                ;;
            -t|--timeout)
                TIMEOUT="$2"
                shift 2
                ;;
            -j|--parallel)
                PARALLEL="$2"
                shift 2
                ;;
            --no-blocklist)
                USE_BLOCKLIST=false
                shift
                ;;
            --extra-blocklist)
                EXTRA_BLOCKLIST_DIRS="$EXTRA_BLOCKLIST_DIRS $2"
                shift 2
                ;;
            --no-whitelist)
                USE_WHITELIST=false
                shift
                ;;
            --whitelist)
                WHITELIST_FILE="$2"
                shift 2
                ;;
            -*)
                print_error "未知选项: $1"
                show_help
                exit 1
                ;;
            *)
                TEST_PATTERNS+=("$1")
                shift
                ;;
        esac
    done
}

# 列出所有测试用例
list_tests() {
    if [ ! -d "$TESTS_DIR" ]; then
        print_error "测试目录不存在: $TESTS_DIR"
        print_info "请先运行 ./download_tests.sh 下载测试套件"
        exit 1
    fi
    
    if [ "$USE_WHITELIST" = true ]; then
        print_info "白名单模式 - 可运行的测试用例 (来自: $WHITELIST_FILE):"
        get_test_list | while read -r test_name; do
            if [ -n "$test_name" ]; then
                echo -e "  ${GREEN}✓${NC} $test_name"
            fi
        done
        
        print_info "所有可用测试用例 (包括未在白名单中的):"
        find "$TESTS_DIR" -name "*_test" -executable | while read -r test_file; do
            local test_name=$(basename "$test_file")
            if is_test_whitelisted "$test_name"; then
                echo -e "  ${GREEN}✓${NC} $test_name (在白名单中)"
            else
                echo -e "  ${YELLOW}○${NC} $test_name (不在白名单中)"
            fi
        done
    else
        print_info "所有可用的测试用例:"
        find "$TESTS_DIR" -name "*_test" -executable | while read -r test_file; do
            local test_name=$(basename "$test_file")
            echo "  $test_name"
        done
    fi
}

# 检查测试套件是否存在
check_test_suite() {
    if [ ! -d "$TESTS_DIR" ] || [ "$(find "$TESTS_DIR" -name "*_test" | wc -l)" -eq 0 ]; then
        print_error "测试套件未找到"
        print_info "请先运行 ./download_tests.sh 下载测试套件"
        exit 1
    fi
}

# 创建必要的目录
setup_directories() {
    mkdir -p "$RESULTS_DIR"
    mkdir -p "$TEMP_DIR"
    mkdir -p "$BLOCKLISTS_DIR"
}

# 读取白名单中的测试程序
get_whitelist_tests() {
    if [ ! -f "$WHITELIST_FILE" ]; then
        print_error "白名单文件不存在: $WHITELIST_FILE"
        return 1
    fi
    
    # 读取白名单文件，忽略注释和空行
    grep -v '^#' "$WHITELIST_FILE" 2>/dev/null | grep -v '^$' | tr -d ' \t'
}

# 检查测试是否在白名单中
is_test_whitelisted() {
    local test_name="$1"
    
    if [ "$USE_WHITELIST" = false ]; then
        return 0  # 不使用白名单时，所有测试都允许
    fi
    
    local whitelisted_tests
    if ! whitelisted_tests=$(get_whitelist_tests); then
        return 1
    fi
    
    # 检查测试是否在白名单中
    echo "$whitelisted_tests" | grep -q "^${test_name}$"
}

# 获取测试的blocklist
get_test_blocklist() {
    local test_name="$1"
    local blocked_subtests=""
    
    if [ "$USE_BLOCKLIST" = false ]; then
        echo ""
        return
    fi
    
    # 检查主blocklist目录
    local blocklist_file="$BLOCKLISTS_DIR/$test_name"
    if [ -f "$blocklist_file" ]; then
        blocked_subtests=$(grep -v '^#' "$blocklist_file" 2>/dev/null | grep -v '^$' | tr '\n' ':')
    fi
    
    # 检查额外的blocklist目录
    for extra_dir in $EXTRA_BLOCKLIST_DIRS; do
        local extra_blocklist="$SCRIPT_DIR/$extra_dir/$test_name"
        if [ -f "$extra_blocklist" ]; then
            local extra_blocked=$(grep -v '^#' "$extra_blocklist" 2>/dev/null | grep -v '^$' | tr '\n' ':')
            blocked_subtests="${blocked_subtests}${extra_blocked}"
        fi
    done
    
    echo "$blocked_subtests"
}

# 运行单个测试
run_single_test() {
    local test_name="$1"
    local test_path="$TESTS_DIR/$test_name"
    
    if [ ! -f "$test_path" ] || [ ! -x "$test_path" ]; then
        print_warn "测试不存在或不可执行: $test_name"
        return 2
    fi
    
    print_test "运行测试用例: $test_name"
    
    # 获取blocklist
    local blocked_subtests=$(get_test_blocklist "$test_name")
    
    # 准备测试环境
    export TEST_TMPDIR="$TEMP_DIR"
    
    # 构建测试命令
    local test_cmd="$test_path"
    if [ -n "$blocked_subtests" ]; then
        test_cmd="$test_cmd --gtest_filter=-$blocked_subtests"
        [ "$VERBOSE" = true ] && print_info "屏蔽的子测试: $blocked_subtests"
    fi
    
    # 执行测试
    local test_output_file="$RESULTS_DIR/${test_name}.output"
    local start_time=$(date +%s)
    
    # 始终将测试输出同时显示到控制台和保存到文件
    # 在verbose模式下显示额外的调试信息
    if [ "$VERBOSE" = true ]; then
        print_info "执行命令: $test_cmd"
        print_info "输出文件: $test_output_file"
    fi
    
    # 使用tee同时输出到控制台和文件
    timeout "$TIMEOUT" bash -c "cd '$TESTS_DIR' && $test_cmd" 2>&1 | tee "$test_output_file"
    local test_result=${PIPESTATUS[0]}
    
    local end_time=$(date +%s)
    local duration=$((end_time - start_time))
    
    # 清理临时文件
    rm -rf "$TEMP_DIR"/*
    
    # 处理测试结果
    case $test_result in
        0)
            print_info "✓ $test_name 通过 (${duration}s)"
            return 0
            ;;
        124)
            print_error "✗ $test_name 超时 (>${TIMEOUT}s)"
            return 1
            ;;
        *)
            print_error "✗ $test_name 失败 (${duration}s)"
            return 1
            ;;
    esac
}

# 获取要运行的测试列表
get_test_list() {
    local all_tests=()
    local candidate_tests=()
    
    # 获取所有测试文件
    while IFS= read -r test_file; do
        all_tests+=($(basename "$test_file"))
    done < <(find "$TESTS_DIR" -name "*_test" -executable | sort)
    
    # 应用白名单过滤
    if [ "$USE_WHITELIST" = true ]; then
        for test in "${all_tests[@]}"; do
            if is_test_whitelisted "$test"; then
                candidate_tests+=("$test")
            fi
        done
        
        if [ ${#candidate_tests[@]} -eq 0 ]; then
            print_warn "没有测试通过白名单过滤"
            return 1
        fi
        
        [ "$VERBOSE" = true ] && print_info "白名单过滤后有 ${#candidate_tests[@]} 个测试可用"
    else
        candidate_tests=("${all_tests[@]}")
    fi
    
    # 如果没有指定模式，返回候选测试
    if [ ${#TEST_PATTERNS[@]} -eq 0 ]; then
        printf '%s\n' "${candidate_tests[@]}"
        return
    fi
    
    # 根据模式过滤测试
    local filtered_tests=()
    for pattern in "${TEST_PATTERNS[@]}"; do
        for test in "${candidate_tests[@]}"; do
            if [[ $test == $pattern ]]; then
                filtered_tests+=("$test")
            fi
        done
    done
    
    printf '%s\n' "${filtered_tests[@]}" | sort -u
}

# 运行所有测试
run_all_tests() {
    local test_list=()
    
    # 获取测试列表
    while IFS= read -r test_name; do
        [ -n "$test_name" ] && test_list+=("$test_name")
    done < <(get_test_list)
    
    if [ ${#test_list[@]} -eq 0 ]; then
        print_warn "没有找到匹配的测试用例"
        return 1
    fi
    
    print_info "准备运行 ${#test_list[@]} 个测试用例"
    
    # 初始化结果文件
    > "$FAILED_CASES_FILE"
    
    # 运行测试
    for test_name in "${test_list[@]}"; do
        ((TOTAL_TESTS++))
        
        if run_single_test "$test_name"; then
            ((PASSED_TESTS++))
        else
            ((FAILED_TESTS++))
            echo "$test_name" >> "$FAILED_CASES_FILE"
        fi
        
        echo "---"
    done
}

# 生成测试报告
generate_report() {
    local report_file="$TEST_REPORT_FILE"
    
    {
        echo "gvisor系统调用测试报告"
        echo "=========================="
        echo "测试时间: $(date)"
        echo "测试目录: $TESTS_DIR"
        echo ""
        echo "测试统计:"
        echo "  总测试数: $TOTAL_TESTS"
        echo "  通过: $PASSED_TESTS"
        echo "  失败: $FAILED_TESTS"
        echo "  成功率: $([ $TOTAL_TESTS -gt 0 ] && echo "scale=2; $PASSED_TESTS * 100 / $TOTAL_TESTS" | bc || echo "0")%"
        echo ""
        
        if [ $FAILED_TESTS -gt 0 ]; then
            echo "失败的测试用例:"
            cat "$FAILED_CASES_FILE" | sed 's/^/  /'
        fi
    } | tee "$report_file"
}

# 显示测试结果
show_results() {
    echo ""
    echo "==============================================="
    print_info "测试完成"
    echo -e "${GREEN}$PASSED_TESTS${NC} / ${GREEN}$TOTAL_TESTS${NC} 测试用例通过"
    
    if [ $FAILED_TESTS -gt 0 ]; then
        echo -e "${RED}$FAILED_TESTS${NC} 个测试用例失败:"
        [ -f "$FAILED_CASES_FILE" ] && cat "$FAILED_CASES_FILE" | sed "s/^/  ${RED}✗${NC} /"
    fi
    
    echo ""
    echo "详细报告保存在: $TEST_REPORT_FILE"
}

# 清理函数
cleanup() {
    print_info "清理临时文件..."
    rm -rf "$TEMP_DIR"
}

# 主函数
main() {
    # 设置清理陷阱
    trap cleanup EXIT
    
    # 解析命令行参数
    parse_args "$@"
    
    # 检查测试套件
    check_test_suite
    
    # 设置目录
    setup_directories
    
    print_info "开始运行gvisor系统调用测试"
    
    # 显示运行配置
    if [ "$USE_WHITELIST" = true ]; then
        print_info "白名单模式已启用: $WHITELIST_FILE"
    fi
    if [ "$USE_BLOCKLIST" = false ]; then
        print_info "黑名单已禁用"
    fi
    
    # 运行测试
    run_all_tests
    
    # 生成报告
    generate_report
    
    # 显示结果
    show_results
    
    # 返回适当的退出码
    [ $FAILED_TESTS -eq 0 ]
}

# 运行主函数
main "$@" 